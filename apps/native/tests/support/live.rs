//! A real daemon for the end-to-end and smoke specs, isolated from the
//! user's: [`dev_daemon::IsolatedDaemon`] puts its config, binaries and
//! worktrees dirs and its tracer pipe prefix under
//! `<repo>/.tmp/native-e2e/<test>-<pid>/`, set on the child process only.
//! [`Harness::open_live`] connects the root view to it through the real
//! network thread.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use daemon_client::device_tokens::MemoryTokens;
use dev_daemon::{DaemonSpec, IsolatedDaemon, TestShell, no_console};
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{Modifiers, TestAppContext, px};
use protocol::{AgentId, ClientMessage, DaemonHandshake, DaemonMessage};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustling_tulip_native::connection_picker::SavedHost;
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{
    Connection, NetCommand, NetDeps, NetEvent, RootDeps, RootView, Transport, bind_keys, spawn_net,
};
use serde_json::{Value, json};

pub use dev_daemon::{kill_process, processes_under};

use super::update_feed::UpdateFeed;
use super::{
    Harness, NotifyRecorder, OpenRecorder, Outbox, PickRecorder, SoundRecorder, TestClock,
};

/// The hang guard of every wait in the live and OS tiers: the daemon writing
/// `daemon.json` and answering `/health`, its processes leaving after a kill,
/// and every [`Harness::wait_until`] / [`Harness::wait_for_row`]. A passing run
/// never reaches it (load-proof-tests); reaching it fails as "hung".
pub const HANG_GUARD: Duration = dev_daemon::HANG_GUARD;
/// The hang guard of [`Harness::wait_until_in_process`], for the in-process
/// UI specs: under nextest's 300 s kill, so a hang fails with its own
/// "hung" message rather than as TIMEOUT.
pub const IN_PROCESS_HANG_GUARD: Duration = Duration::from_secs(120);
/// The id file the native client keeps its identity in.
const CLIENT_ID_FILE: &str = "client-id-native";

/// The repository root, for the `.tmp` dir and `tools/e2e/fake-tailscale`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("apps/native sits two levels under the repo root")
        .to_path_buf()
}

/// The fake Claude CLI's script, which a spec runs under `node` itself.
pub fn fake_claude_script() -> PathBuf {
    repo_root()
        .join("crates")
        .join("agents")
        .join("claude")
        .join("fake")
        .join("index.mjs")
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

/// A running `rustling-tulipd` with every directory under this test's own
/// scratch dir. Drop kills it and the tracers it spawned, then removes the
/// dir.
pub struct LiveDaemon {
    inner: IsolatedDaemon,
    /// The DER of the test CA behind the fake `tailscale cert`'s leaf, for a
    /// daemon from [`Self::start_remote`].
    tailnet_ca: Option<Vec<u8>>,
}

impl LiveDaemon {
    /// Starts a daemon for `test` and waits for its handshake and `/health`.
    pub fn start(test: &str) -> Self {
        Self::start_with_env(test, &[])
    }

    /// [`Self::start`], with `extra` set on the daemon's process after the
    /// isolation variables (so a row can override one) and kept in
    /// [`Self::envs`].
    pub fn start_with_env(test: &str, extra: &[(&'static str, OsString)]) -> Self {
        Self::spawn_in(
            DaemonSetup::target(scratch_root(test), test),
            extra,
            None,
            Vec::new(),
        )
    }

    /// A daemon run from a scratch install of its own, in the host-update
    /// sandbox: `rustling-tulipd` and `rt-tracer` are copied from
    /// [`target_bin_dir`] into `<scratch root>/install`, which the daemon
    /// runs from, and its daemon-only environment (never in [`Self::envs`],
    /// so no bridge or CLI verb sees it) sets `RUSTLING_TULIP_E2E_HOST_UPDATE=1`,
    /// `feed` as its release feed and, when given, the countdown in
    /// seconds. `remote` sets up the fake tailnet as [`Self::start_remote`]
    /// does. The daemon needs the `e2e-loopback-peers` build.
    #[expect(
        clippy::panic,
        reason = "a scratch install that cannot be made fails the spec with the copy's own error"
    )]
    pub fn start_update_host(
        test: &str,
        feed: &UpdateFeed,
        countdown_secs: Option<u32>,
        remote: bool,
    ) -> Self {
        let root = scratch_root(test);
        let install = root.join("install");
        std::fs::create_dir_all(&install).expect("create the scratch install");
        let source = target_bin_dir();
        for name in ["rustling-tulipd", "rt-tracer"] {
            let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            std::fs::copy(source.join(&file), install.join(&file))
                .unwrap_or_else(|err| panic!("copy {file} into the scratch install: {err}"));
        }
        let mut daemon_only = vec![
            ("RUSTLING_TULIP_E2E_HOST_UPDATE", OsString::from("1")),
            (
                "RUSTLING_TULIP_UPDATE_FEED",
                OsString::from(feed.base_url()),
            ),
        ];
        if let Some(secs) = countdown_secs {
            daemon_only.push((
                "RUSTLING_TULIP_E2E_HOST_UPDATE_COUNTDOWN_SECS",
                OsString::from(secs.to_string()),
            ));
        }
        let setup = DaemonSetup {
            root,
            test,
            bin_dir: install,
        };
        if !remote {
            return Self::spawn_in(setup, &[], None, daemon_only);
        }
        let (envs, ca) = fake_tailnet(&setup.root);
        Self::spawn_in(setup, &envs, Some(ca), daemon_only)
    }

    /// [`Self::start`] for remote access: the daemon's `tailscale` is
    /// `tools/e2e/fake-tailscale`, answering as the host [`REMOTE_HOST`]
    /// whose owner every peer is, and its `cert` hands out a leaf for
    /// [`REMOTE_HOST`] signed by a test CA that [`Self::tailnet_tls`] trusts.
    /// Remote access stays off until a client sends `ConfigureRemote`; the
    /// daemon needs the `e2e-loopback-peers` build to admit a loopback dial.
    pub fn start_remote(test: &str) -> Self {
        Self::start_remote_with_env(test, &[])
    }

    /// [`Self::start_remote`], with `extra` set on the daemon's process after
    /// the isolation variables and the fake tailscale's state (so a row can
    /// override one) and kept in [`Self::envs`].
    pub fn start_remote_with_env(test: &str, extra: &[(&'static str, OsString)]) -> Self {
        let setup = DaemonSetup::target(scratch_root(test), test);
        let (mut envs, ca) = fake_tailnet(&setup.root);
        envs.extend_from_slice(extra);
        Self::spawn_in(setup, &envs, Some(ca), Vec::new())
    }

    /// Starts the daemon over `setup`'s root, from [`scratch_root`], with the
    /// daemon and tracer in its `bin_dir`, `extra` kept in [`Self::envs`] and
    /// `daemon_only` set on the daemon's process alone, and waits for its
    /// handshake and `/health`.
    #[expect(
        clippy::panic,
        reason = "a daemon that does not start fails the spec with the launcher's own message"
    )]
    fn spawn_in(
        setup: DaemonSetup<'_>,
        extra: &[(&'static str, OsString)],
        tailnet_ca: Option<Vec<u8>>,
        daemon_only: Vec<(&'static str, OsString)>,
    ) -> Self {
        let spec = DaemonSpec {
            name: setup.test.to_owned(),
            scratch_root: setup.root,
            repo_root: repo_root(),
            bin_dir: setup.bin_dir,
            extra_env: extra.to_vec(),
            daemon_only_env: daemon_only,
            shell: TestShell::Cmd,
        };
        let inner = IsolatedDaemon::start(spec).unwrap_or_else(|err| panic!("{err}"));
        Self { inner, tailnet_ca }
    }

    /// Stops the daemon as the installer would, with `rustling-tulipd
    /// shutdown`, writes `last_result` as `apply-update --restart-daemon`
    /// does, to both `<updates dir>/last-result.json` (the host app's) and
    /// `<updates dir>/host-update-result.json` (the daemon's), and starts a
    /// new daemon on the same directories and environment, waiting for its
    /// handshake and `/health`.
    pub fn restart_after_update(&mut self, last_result: &Value) {
        self.restart_with_result(last_result, false);
    }

    /// [`Self::restart_after_update`], but `last-result.json` is deleted
    /// before the new daemon starts, as the host's relaunched app takes it
    /// before it starts the daemon.
    pub fn restart_after_update_app_took_its_own(&mut self, last_result: &Value) {
        self.restart_with_result(last_result, true);
    }

    #[expect(
        clippy::panic,
        reason = "a daemon that does not start again fails the spec with the launcher's own message"
    )]
    fn restart_with_result(&mut self, last_result: &Value, app_took_its_own: bool) {
        self.shut_down_by_cli();
        let updates = self.updates_dir();
        std::fs::create_dir_all(&updates).expect("create the updates dir");
        for name in ["last-result.json", "host-update-result.json"] {
            std::fs::write(updates.join(name), last_result.to_string())
                .unwrap_or_else(|err| panic!("write {name}: {err}"));
        }
        if app_took_its_own {
            std::fs::remove_file(updates.join("last-result.json"))
                .expect("the app takes last-result.json");
        }
        self.inner.respawn().unwrap_or_else(|err| panic!("{err}"));
    }

    /// The daemon's `RUSTLING_TULIP_UPDATES_DIR`, from [`Self::envs`].
    pub fn updates_dir(&self) -> PathBuf {
        self.envs()
            .iter()
            .rev()
            .find(|(key, _)| *key == "RUSTLING_TULIP_UPDATES_DIR")
            .map(|(_, value)| PathBuf::from(value))
            .expect("an isolated daemon has an updates dir")
    }

    /// The daemon's live-run folder, `RUSTLING_TULIP_DISPATCH_DIR`, from
    /// [`Self::envs`]: a dispatch run's file written here is listed.
    pub fn dispatch_dir(&self) -> PathBuf {
        self.envs()
            .iter()
            .rev()
            .find(|(key, _)| *key == "RUSTLING_TULIP_DISPATCH_DIR")
            .map(|(_, value)| PathBuf::from(value))
            .expect("an isolated daemon has a live-run folder")
    }

    /// Makes the fake tailscale's `whois` answer that every peer belongs to
    /// a user other than the host's owner, for a daemon from
    /// [`Self::start_remote`]. The shim reads its state on every run, so the
    /// next `whois` already answers so.
    pub fn make_peers_non_owners(&self) {
        let path = self.dir().join("fake-tailscale").join("state.json");
        let text = std::fs::read_to_string(&path).expect("read the fake tailscale's state");
        let mut state: Value =
            serde_json::from_str(&text).expect("the fake tailscale's state is JSON");
        state["whois_user_id"] = json!(OWNER_USER_ID + 1);
        // Written beside it and renamed over, so a shim run mid-write never
        // reads a truncated file.
        let staged = path.with_extension("json.tmp");
        std::fs::write(&staged, state.to_string()).expect("write the fake tailscale's new state");
        std::fs::rename(&staged, &path).expect("replace the fake tailscale's state");
    }

    /// The daemon's log as written so far; empty before the daemon made the
    /// file, which a poll for a line reads as "not logged yet".
    pub fn daemon_log(&self) -> String {
        self.inner
            .daemon_log()
            .map_err(|err| format!("{}: {err}", self.config_dir().join("logs").display()))
            .expect("reading the daemon log")
    }

    /// Whether the daemon still answers `GET /health` on its loopback port.
    pub fn answers_health(&self) -> bool {
        self.inner.answers_health()
    }

    /// The test's scratch dir; fixtures go here.
    pub fn dir(&self) -> &Path {
        self.inner.dir()
    }

    pub fn config_dir(&self) -> PathBuf {
        self.inner.config_dir()
    }

    /// The home folder `agent`'s fake CLI names in its `home_env`: where the
    /// daemon and the sessions it spawns look for that agent's files, in
    /// place of the user's own.
    pub fn agent_home(&self, agent: &AgentId) -> PathBuf {
        self.inner.agent_home(agent)
    }

    /// The daemon's binary cache: the tracers, and a daemon it respawns,
    /// run from copies here named `<name>-<hash>.exe`.
    pub fn binaries_dir(&self) -> PathBuf {
        self.inner.binaries_dir()
    }

    /// The daemon's `RUSTLING_TULIP_TAILNET_DIR`, where `tailscale cert`
    /// writes the listener's certificate.
    pub fn tailnet_dir(&self) -> PathBuf {
        self.inner.tailnet_dir()
    }

    /// A TLS client config whose only root is the test CA of a daemon from
    /// [`Self::start_remote`], for a tailnet dial to it.
    pub fn tailnet_tls(&self) -> Arc<rustls::ClientConfig> {
        let der = self
            .tailnet_ca
            .clone()
            .expect("only a daemon from start_remote has a tailnet test CA");
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(der))
            .expect("the test CA is a usable root");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("the ring provider supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(config)
    }

    /// The variables that isolate a process to this daemon, to set on a
    /// child with `Command::envs`.
    pub fn envs(&self) -> &[(&'static str, OsString)] {
        self.inner.envs()
    }

    pub fn handshake(&self) -> &DaemonHandshake {
        self.inner.handshake()
    }

    /// Force-kills the daemon alone, leaving its tracers running, as a crash
    /// would.
    pub fn kill(&mut self) {
        self.inner.kill().expect("kill and reap rustling-tulipd");
    }

    /// Kills the daemon and every tracer it started, as a reboot would, then
    /// starts a new daemon on the same directories.
    #[expect(
        clippy::panic,
        reason = "a daemon that does not start again fails the spec with the launcher's own message"
    )]
    pub fn restart_after_reboot(&mut self) {
        self.kill();
        let binaries = self.binaries_dir();
        for (pid, _) in processes_under(&binaries) {
            kill_process(pid);
        }
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            let running = processes_under(&binaries);
            if running.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "hung: processes still run from {} {HANG_GUARD:?} after the kill: {running:?}",
                binaries.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        self.inner.respawn().unwrap_or_else(|err| panic!("{err}"));
    }

    /// Stops the daemon the way a user would, with `rustling-tulipd shutdown`
    /// under its isolation variables, and waits for its process to exit.
    pub fn shut_down_by_cli(&mut self) {
        let mut command = Command::new(self.inner.daemon_exe());
        command
            .arg("shutdown")
            .envs(self.envs().iter().map(|(key, value)| (*key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_console(&mut command);
        let status = command.status().expect("run rustling-tulipd shutdown");
        assert!(
            status.success(),
            "rustling-tulipd shutdown failed: {status}"
        );
        let deadline = Instant::now() + HANG_GUARD;
        while self
            .inner
            .child_mut()
            .try_wait()
            .expect("poll rustling-tulipd")
            .is_none()
        {
            assert!(
                Instant::now() < deadline,
                "hung: rustling-tulipd still runs {HANG_GUARD:?} after its shutdown"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The fake `ssh` for this daemon: its state file under the test dir
    /// names the daemon binary beside the test binary as the bridge, every
    /// isolation variable of this daemon as the bridge's environment (so a
    /// daemon the bridge starts is isolated the same way), and `fail` as a
    /// refused login's exit code and stderr.
    ///
    /// Bind the result after the daemon, so it drops first: its drop makes
    /// every later run refuse the login, so a client still reconnecting
    /// while the daemon is torn down never has a bridge start a new daemon
    /// that nothing stops.
    pub fn fake_ssh(&self, fail: Option<(i32, &str)>) -> FakeSsh {
        let dir = self.dir().join("fake-ssh");
        std::fs::create_dir_all(&dir).expect("create the fake ssh's dir");
        let env: serde_json::Map<String, Value> = self
            .envs()
            .iter()
            .map(|(key, value)| ((*key).to_owned(), json!(value.to_string_lossy())))
            .collect();
        let log = dir.join("argv.jsonl");
        let state = json!({
            "bridge": self.inner.daemon_exe(),
            "env": env,
            "fail": fail.map(|(code, stderr)| json!({ "code": code, "stderr": stderr })),
            "log": log,
        });
        let state_file = dir.join("state.json");
        std::fs::write(&state_file, state.to_string()).expect("write the fake ssh's state");
        let shim = if cfg!(windows) {
            "fake-ssh.cmd"
        } else {
            "fake-ssh.sh"
        };
        FakeSsh {
            shim: repo_root()
                .join("tools")
                .join("e2e")
                .join("fake-ssh")
                .join(shim),
            state: state_file,
            log,
        }
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
        let repo = self.dir().join("fixture");
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

/// Commits every change in `repo`'s worktree, as [`LiveDaemon::git_fixture`]'s
/// author.
pub fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "-c",
            "user.name=rt-e2e",
            "-c",
            "user.email=rt-e2e@example.invalid",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

/// The variable naming the fake ssh's state file.
const FAKE_SSH_STATE_ENV: &str = "RUSTLING_TULIP_FAKE_SSH_STATE";

/// `tools/e2e/fake-ssh` set up for one test daemon by
/// [`LiveDaemon::fake_ssh`]: its shim, its state file and its argv log.
pub struct FakeSsh {
    shim: PathBuf,
    state: PathBuf,
    log: PathBuf,
}

/// One run of the fake ssh, as its argv log records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshRun {
    /// The shim's own process: killing its tree cuts the connection.
    pub pid: u32,
    pub argv: Vec<String>,
}

/// The stderr every run refuses with once its [`FakeSsh`] is dropped.
const FAKE_SSH_ENDED: &str = "fake-ssh: the spec has ended";

impl Drop for FakeSsh {
    /// Rewrites the state to refuse every later login, keeping the file for
    /// a failed spec's dir.
    fn drop(&mut self) {
        let rewritten = std::fs::read_to_string(&self.state)
            .map_err(|err| err.to_string())
            .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|err| err.to_string()))
            .and_then(|mut state| {
                state["fail"] = json!({ "code": 255, "stderr": FAKE_SSH_ENDED });
                // Written beside it and renamed over, so a shim run
                // mid-write never reads a truncated file.
                let staged = self.state.with_extension("json.tmp");
                std::fs::write(&staged, state.to_string()).map_err(|err| err.to_string())?;
                std::fs::rename(&staged, &self.state).map_err(|err| err.to_string())
            });
        if let Err(err) = rewritten {
            tracing::warn!(
                "could not make the fake ssh at {} refuse logins: {err}",
                self.state.display()
            );
        }
    }
}

impl FakeSsh {
    /// The client's variables for this fake: `RUSTLING_TULIP_SSH` names the
    /// shim, and the shim finds its state through the other.
    pub fn envs(&self) -> [(&'static str, OsString); 2] {
        [
            (
                rustling_tulip_native::ssh::SSH_ENV,
                self.shim.clone().into_os_string(),
            ),
            (FAKE_SSH_STATE_ENV, self.state.clone().into_os_string()),
        ]
    }

    /// Sets [`Self::envs`] on this test process, which the client's `ssh`
    /// child inherits. Each nextest test runs in a process of its own, so the
    /// variables reach no other spec.
    pub fn use_for_this_process(&self) {
        for (key, value) in self.envs() {
            // SAFETY: set before the spec opens its client, and the threads
            // already running read the environment only through std, which
            // serializes its reads and writes with one lock.
            unsafe { std::env::set_var(key, value) };
        }
    }

    /// Makes every later run refuse the login with `fail`'s exit code and
    /// stderr, or, with `None`, run the bridge again. The shim reads its
    /// state on every run, so the next run already answers so.
    pub fn set_fail(&self, fail: Option<(i32, &str)>) {
        let text = std::fs::read_to_string(&self.state)
            .map_err(|err| format!("{}: {err}", self.state.display()))
            .expect("read the fake ssh's state");
        let mut state: Value = serde_json::from_str(&text).expect("the fake ssh's state is JSON");
        state["fail"] = fail.map_or(
            Value::Null,
            |(code, stderr)| json!({ "code": code, "stderr": stderr }),
        );
        // Written beside it and renamed over, so a shim run mid-write never
        // reads a truncated file.
        let staged = self.state.with_extension("json.tmp");
        std::fs::write(&staged, state.to_string()).expect("write the fake ssh's new state");
        std::fs::rename(&staged, &self.state).expect("replace the fake ssh's state");
    }

    /// Every run of the shim so far, oldest first; empty before the first.
    pub fn runs(&self) -> Vec<SshRun> {
        let text = match std::fs::read_to_string(&self.log) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            read => read
                .map_err(|err| format!("{}: {err}", self.log.display()))
                .expect("reading the fake ssh's argv log"),
        };
        text.lines()
            .filter(|line| line.ends_with('}'))
            .map(|line| {
                let run: Value = serde_json::from_str(line).expect("an argv log line is JSON");
                SshRun {
                    pid: run["pid"]
                        .as_u64()
                        .and_then(|pid| u32::try_from(pid).ok())
                        .expect("an argv log line names its pid"),
                    argv: serde_json::from_value(run["argv"].clone())
                        .expect("an argv log line holds its argv"),
                }
            })
            .collect()
    }
}

/// The `MagicDNS` name the fake tailscale gives the host, without its trailing
/// dot: the name its certificate is for and a tailnet client dials.
pub const REMOTE_HOST: &str = "localhost";
/// The owner's Tailscale user id in the fake tailnet.
const OWNER_USER_ID: i64 = 2;
/// The variable naming the fake tailscale's state file.
const FAKE_TAILSCALE_STATE_ENV: &str = "RUSTLING_TULIP_FAKE_TAILSCALE_STATE";

/// Where [`LiveDaemon::spawn_in`] starts a daemon: its scratch root, the
/// test's name and the folder it runs from.
struct DaemonSetup<'a> {
    root: PathBuf,
    test: &'a str,
    bin_dir: PathBuf,
}

impl<'a> DaemonSetup<'a> {
    /// `root` for `test`, the daemon run from beside the test binary.
    fn target(root: PathBuf, test: &'a str) -> Self {
        Self {
            root,
            test,
            bin_dir: target_bin_dir(),
        }
    }
}

/// Sets up `tools/e2e/fake-tailscale` under `root`: a test CA and its leaf
/// for [`REMOTE_HOST`], and the shim's state naming every peer the host's
/// owner. The daemon's variable for that state, and the CA's DER.
fn fake_tailnet(root: &Path) -> (Vec<(&'static str, OsString)>, Vec<u8>) {
    let fake = root.join("fake-tailscale");
    std::fs::create_dir_all(&fake).expect("create the fake tailscale's dir");
    let ca = TestCa::new();
    let (cert_pem, key_pem) = ca.leaf();
    let cert_file = fake.join("leaf.crt");
    let key_file = fake.join("leaf.key");
    std::fs::write(&cert_file, cert_pem).expect("write the test leaf");
    std::fs::write(&key_file, key_pem).expect("write the test leaf's key");
    let state = json!({
        "user_id": OWNER_USER_ID,
        "cert_file": cert_file,
        "key_file": key_file,
    });
    let state_file = fake.join("state.json");
    std::fs::write(&state_file, state.to_string()).expect("write the fake tailscale's state");
    let envs = vec![(FAKE_TAILSCALE_STATE_ENV, state_file.into_os_string())];
    (envs, ca.der)
}

/// `<repo>/.tmp/native-e2e/<test>-<pid>/`, emptied; [`IsolatedDaemon::start`]
/// makes the daemon's directories, each registered agent's home folder and
/// its `PATH` stub in it.
fn scratch_root(test: &str) -> PathBuf {
    let root = repo_root()
        .join(".tmp")
        .join("native-e2e")
        .join(format!("{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

/// A test certificate authority for the fake `tailscale cert`'s leaf.
struct TestCa {
    der: Vec<u8>,
    issuer: Issuer<'static, KeyPair>,
}

impl TestCa {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "rustling-tulip e2e test CA");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let key = KeyPair::generate().expect("CA key");
        let cert = params.self_signed(&key).expect("self-signed CA");
        Self {
            der: cert.der().to_vec(),
            issuer: Issuer::new(params, key),
        }
    }

    /// A leaf for [`REMOTE_HOST`] signed by this CA, and its key, as PEM.
    /// rcgen's default validity runs to the year 4096, so the daemon never
    /// finds it near expiry.
    fn leaf(&self) -> (String, String) {
        let mut params = CertificateParams::new(vec![REMOTE_HOST.to_owned()]).expect("leaf params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("leaf key");
        let cert = params
            .signed_by(&key, &self.issuer)
            .expect("leaf signed by the CA");
        (cert.pem(), key.serialize_pem())
    }
}

/// What a `PATH` stub prints before its arguments.
pub const STUB_MARKER: &str = dev_daemon::STUB_MARKER;

/// Force-kills `pid` and, on Windows, every process under it.
pub fn kill_tree(pid: u32) {
    dev_daemon::kill_tree(pid);
}

/// A loopback port nothing listens on right now.
pub fn free_port() -> u16 {
    dev_daemon::free_port().expect("bind a free loopback port")
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

/// The client side of a live harness: the root view's own command sender and
/// every connection state the network thread reported to the view.
#[derive(Clone)]
pub struct LiveClient {
    tx: UnboundedSender<NetCommand>,
    states: Arc<Mutex<Vec<Connection>>>,
    messages: Arc<Mutex<Vec<DaemonMessage>>>,
}

impl LiveClient {
    /// The first answer `pick` gives, asking it of every message the daemon
    /// sent this client, newest first.
    pub fn find_message<R>(&self, pick: impl FnMut(&DaemonMessage) -> Option<R>) -> Option<R> {
        self.messages
            .lock()
            .expect("message log lock")
            .iter()
            .rev()
            .find_map(pick)
    }

    /// Removes the newest message `pick` answers from the log and returns the
    /// answer, so a poll that asks for a large reply again and again does not
    /// grow the log. It shifts later indices, so a caller of
    /// [`Self::find_message_from`] never takes messages.
    pub fn take_message<R>(&self, mut pick: impl FnMut(&DaemonMessage) -> Option<R>) -> Option<R> {
        let mut messages = self.messages.lock().expect("message log lock");
        let (index, answer) = messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, msg)| pick(msg).map(|answer| (index, answer)))?;
        messages.remove(index);
        Some(answer)
    }

    /// How many messages the daemon has sent this client so far.
    pub fn message_count(&self) -> usize {
        self.messages.lock().expect("message log lock").len()
    }

    /// The first answer `pick` gives among the messages from index `from` on,
    /// newest first.
    pub fn find_message_from<R>(
        &self,
        from: usize,
        pick: impl FnMut(&DaemonMessage) -> Option<R>,
    ) -> Option<R> {
        self.messages
            .lock()
            .expect("message log lock")
            .iter()
            .skip(from)
            .rev()
            .find_map(pick)
    }

    /// Sends `msg` to the daemon as the client would.
    pub fn send(&self, msg: ClientMessage) {
        self.command(NetCommand::Send(Box::new(msg)));
    }

    /// Hands `command` to the network thread, as the view does.
    pub fn command(&self, command: NetCommand) {
        self.tx
            .unbounded_send(command)
            .expect("the network thread is running");
    }

    /// Every state the view was sent, oldest first.
    pub fn states(&self) -> Vec<Connection> {
        self.states.lock().expect("state log lock").clone()
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
    let target = json!({ "kind": "standalone", "cwd": cwd });
    spawn_message(&target, "plain_shell", &Value::Null)
}

/// An interactive session of the default agent on `main`, in place in
/// `repo_id`'s checkout.
pub fn spawn_default_agent_in_place(repo_id: &str) -> ClientMessage {
    let target = json!({
        "kind": "single",
        "repo_id": repo_id,
        "branch_name": "main",
        "base_branch": null,
        "use_worktree": false,
    });
    let agent = json!({ "agent": agent_registry::default_agent(), "options": {} });
    spawn_message(&target, "interactive", &agent)
}

/// A spawn of `mode` on `target` running `agent` (null for a plain shell).
fn spawn_message(target: &Value, mode: &str, agent: &Value) -> ClientMessage {
    serde_json::from_value(json!({
        "type": "spawn_session",
        "label": null,
        "target": target,
        "mode": mode,
        "initial_prompt": null,
        "dangerously_skip_permissions": false,
        "agent": agent,
    }))
    .expect("spawn request fixture")
}

impl<'a> Harness<'a> {
    /// Opens the window on the real network thread, connected to `daemon`
    /// through its isolated handshake. The fake-daemon half of the harness
    /// ([`Self::send`], [`Self::sent`]) does nothing here.
    pub fn open_live(cx: &'a mut TestAppContext, daemon: &LiveDaemon) -> (Self, LiveClient) {
        let tokens = Arc::new(MemoryTokens::default());
        let (client, view_events) =
            live_client(&daemon.config_dir(), Target::Local, None, tokens.clone());
        let first = (client.tx.clone(), view_events);
        let harness = Self::open_live_with(cx, daemon.dir().join("ui"), tokens, |opened| {
            super::fake_transport(first, opened)
        });
        (harness, client)
    }

    /// Opens the window as production does: This PC's connection goes to
    /// `home`, and each of `remotes` is saved in `home`'s `native-ui.json`
    /// first, so the window dials it at start through its saved hosts. Every
    /// host runs its own real network thread; [`LiveHosts`] holds the client
    /// side of each. `tokens` are the device tokens the view and every
    /// connection share. A tailnet host whose name a saved host may not hold
    /// (the fake tailnet's bare [`REMOTE_HOST`]) is dialled once the window
    /// is open, as the Connections picker dials one, since the window drops
    /// such a saved host unconnected.
    pub fn open_hosts(
        cx: &'a mut TestAppContext,
        home: &LiveDaemon,
        remotes: Vec<LiveHostSpec>,
        tokens: Arc<MemoryTokens>,
    ) -> (Self, LiveHosts) {
        Self::open_hosts_switched(cx, home, remotes, &[], tokens)
    }

    /// [`Self::open_hosts`], with each host of `remotes` whose key is in
    /// `off` saved switched off: the window does not dial it at start, and
    /// a spec switches it on through Connections.
    #[expect(
        clippy::panic,
        reason = "a window opening a host the spec did not list is a bug in the spec"
    )]
    pub fn open_hosts_switched(
        cx: &'a mut TestAppContext,
        home: &LiveDaemon,
        remotes: Vec<LiveHostSpec>,
        off: &[HostKey],
        tokens: Arc<MemoryTokens>,
    ) -> (Self, LiveHosts) {
        let ui_dir = home.dir().join("ui");
        seed_saved_hosts(&ui_dir, &remotes, off);
        let dialled: Vec<Target> = remotes
            .iter()
            .filter(|spec| !can_be_saved(spec))
            .map(|spec| spec.target.clone())
            .collect();
        let connecting: Vec<HostKey> = std::iter::once(HostKey::Local)
            .chain(
                remotes
                    .iter()
                    .filter(|spec| spec.connects)
                    .map(|spec| HostKey::of(&spec.target))
                    .filter(|key| !off.contains(key)),
            )
            .collect();
        let hosts = LiveHosts::default();
        let home_config = home.config_dir();
        let shared_hosts = hosts.clone();
        let net_tokens = tokens.clone();
        let mut harness = Self::open_live_with(cx, ui_dir, tokens, move |opened| {
            let opened = Rc::clone(opened);
            Rc::new(move |target: &Target| {
                opened.borrow_mut().push(target.clone());
                let key = HostKey::of(target);
                let (config, tls) = if key == HostKey::Local {
                    (home_config.clone(), None)
                } else {
                    let spec = remotes
                        .iter()
                        .find(|spec| HostKey::of(&spec.target) == key)
                        .unwrap_or_else(|| {
                            panic!("the window opened {target:?}, which the spec did not list")
                        });
                    (spec.config_dir.clone(), spec.tls.clone())
                };
                let (client, view_events) =
                    live_client(&config, target.clone(), tls, net_tokens.clone());
                let tx = client.tx.clone();
                shared_hosts
                    .0
                    .borrow_mut()
                    .entry(key)
                    .or_default()
                    .push(client);
                (tx, view_events)
            })
        });
        for target in dialled {
            harness.root_update(move |view, window, cx| view.connect_host(target, window, cx));
        }
        harness.start_empty_layouts(&hosts, &connecting);
        (harness, hosts)
    }

    /// Waits until each of `keys`' daemons has sent this client its tab
    /// layout, taking Start empty on the first-connect chooser whenever one
    /// shows (a question with nothing else to offer is answered without
    /// asking), so a spec's clicks reach the window.
    fn start_empty_layouts(&mut self, hosts: &LiveHosts, keys: &[HostKey]) {
        let clients: Vec<LiveClient> = keys.iter().map(|key| hosts.client(key)).collect();
        let has_tabs = |client: &LiveClient| {
            client
                .find_message(|msg| matches!(msg, DaemonMessage::Tabs { .. }).then_some(()))
                .is_some()
        };
        self.wait_until(
            &format!("a tab layout for each of {keys:?}, with no chooser left open"),
            |h| {
                if h.root(|root, _| root.layout_chooser_open()) {
                    h.click_on("layout-choose-empty");
                }
                clients.iter().all(has_tabs) && !h.root(|root, _| root.layout_chooser_open())
            },
        );
    }

    /// Opens the window over `open_host`, which `make_transport` builds
    /// around the list of every target the window opens, with its UI state
    /// in `ui_dir`.
    fn open_live_with(
        cx: &'a mut TestAppContext,
        ui_dir: PathBuf,
        tokens: Arc<MemoryTokens>,
        make_transport: impl FnOnce(&Rc<RefCell<Vec<Target>>>) -> Transport,
    ) -> Self {
        // The real net thread wakes the app, which gpui's test scheduler
        // refuses unless the spec allows parking.
        cx.executor().allow_parking();
        cx.update(bind_keys);
        let clock = TestClock::new();
        let quits = Rc::new(std::cell::Cell::new(0));
        let opener = Arc::new(OpenRecorder::default());
        let notifier = Arc::new(NotifyRecorder::default());
        let placer = Arc::new(super::PlaceRecorder::default());
        let sounder = Arc::new(SoundRecorder::default());
        let badge = Rc::new(super::BadgeRecorder::default());
        let picker = Rc::new(PickRecorder::default());
        let autostart = Arc::new(super::AutostartRecorder::default());
        let updater = Arc::new(super::UpdaterRecorder::default());
        let opened = Rc::new(RefCell::new(Vec::new()));
        let deps = RootDeps {
            start_mode: rustling_tulip_native::start_guard::StartMode::Normal,
            open_host: make_transport(&opened),
            ui_dir: Some(ui_dir),
            paths: Err("live specs have no log paths".to_owned()),
            wanted: None,
            now: clock.clock(),
            quit: super::quit_recorder(&quits),
            open: opener.clone(),
            notify: notifier.clone(),
            toast_placer: placer.clone(),
            sound: sounder.clone(),
            badge: badge.clone(),
            pick_folder: PickRecorder::picker(&picker),
            pick_file: PickRecorder::file_picker(&picker),
            pick_save: PickRecorder::save_picker(&picker),
            autostart: autostart.clone(),
            tokens: tokens.clone(),
            updater: updater.clone(),
            shape: rustling_tulip_native::RootShape::Desktop,
        };
        let (root, cx) =
            cx.add_window_view(move |window, cx| RootView::with_transport(deps, window, cx));
        Self {
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
            placer,
            sounder,
            badge,
            picker,
            autostart,
            tokens,
            updater,
            extra_hosts: super::ExtraHosts::default(),
        }
    }

    /// Clicks `+ Shell` on `host`'s header in the sidebar and returns the
    /// pane its shell lands in, once that pane shows a session listed under
    /// `host`'s group.
    pub fn place_shell_on(&mut self, host: &HostKey) -> String {
        let button = format!("host-shell-{}", host.wire().unwrap_or("local"));
        self.wait_until(&format!("the {button} button"), |h| {
            h.bounds(&button).size.height > px(0.0)
        });
        self.click_on(&button);
        let mut placed = None;
        self.wait_until(&format!("a pane holding the shell {button} spawned"), |h| {
            placed = h.first_pane().and_then(|pane| {
                let session = h.root(|root, _| root.pane_session(&pane))?;
                let listed = h.root(|root, _| {
                    root.sidebar_containers().iter().any(|container| {
                        container.host == *host
                            && container.leaves.iter().any(|leaf| leaf.id == session)
                    })
                });
                listed.then_some(pane)
            });
            placed.is_some()
        });
        placed.expect("the wait just found it")
    }
}

/// A remote host a window opened by [`Harness::open_hosts`] connects to:
/// its target, the config dir of the daemon it reaches (where the client
/// keeps its identity), the roots a tailnet dial trusts (`None` for the
/// OS store) and whether the spec expects the connection to open, which
/// [`Harness::open_hosts`] then waits for.
pub struct LiveHostSpec {
    pub target: Target,
    pub config_dir: PathBuf,
    pub tls: Option<Arc<rustls::ClientConfig>>,
    pub connects: bool,
}

/// The client side of every connection a window from
/// [`Harness::open_hosts`] opened, by host; a host opened again keeps each
/// connection, newest last.
#[derive(Clone, Default)]
pub struct LiveHosts(Rc<RefCell<HashMap<HostKey, Vec<LiveClient>>>>);

impl LiveHosts {
    /// The newest connection the window opened to `host`.
    #[expect(
        clippy::panic,
        reason = "a spec asking for a host the window never opened is a bug in the spec"
    )]
    pub fn client(&self, host: &HostKey) -> LiveClient {
        self.0
            .borrow()
            .get(host)
            .and_then(|clients| clients.last())
            .cloned()
            .unwrap_or_else(|| panic!("the window opened no connection to {host:?}"))
    }
}

/// Whether the window accepts `spec`'s host as a saved host: every SSH host,
/// and a tailnet host whose name is a full `MagicDNS` name.
fn can_be_saved(spec: &LiveHostSpec) -> bool {
    spec.target
        .tailnet_host()
        .is_none_or(|host| Target::parse_tailnet(host.name(), Some(host.port())).is_ok())
}

/// Writes those of `remotes` the window accepts as saved hosts into the
/// `native-ui.json` in `ui_dir`, those whose key is in `off` switched off,
/// keeping every other key a spec seeded there first.
fn seed_saved_hosts(ui_dir: &Path, remotes: &[LiveHostSpec], off: &[HostKey]) {
    let path = ui_dir.join("native-ui.json");
    let mut state = match std::fs::read_to_string(&path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({}),
        read => {
            let text = read
                .map_err(|err| format!("{}: {err}", path.display()))
                .expect("reading the seeded native-ui.json");
            serde_json::from_str(&text).expect("the seeded native-ui.json is JSON")
        }
    };
    let saved: Vec<Value> = remotes
        .iter()
        .filter(|spec| can_be_saved(spec))
        .map(|spec| {
            let mut host =
                SavedHost::from_target(&spec.target).expect("a remote host is not This PC");
            host.enabled = !off.contains(&HostKey::of(&spec.target));
            serde_json::to_value(host).expect("a saved host serializes")
        })
        .collect();
    state["saved_hosts"] = Value::Array(saved);
    std::fs::write(&path, state.to_string()).expect("write native-ui.json's saved hosts");
}

/// A real network thread to the daemon under `config` on `target`, its
/// events recorded into the returned client on the way to the returned
/// receiver, which the view reads.
fn live_client(
    config: &Path,
    target: Target,
    tls: Option<Arc<rustls::ClientConfig>>,
    tokens: Arc<MemoryTokens>,
) -> (LiveClient, UnboundedReceiver<NetEvent>) {
    let (tx, net_commands) = unbounded();
    let (net_events, from_net) = unbounded();
    let (to_view, view_events) = unbounded();
    spawn_net(
        live_deps(config, tls, tokens),
        target,
        net_commands,
        net_events,
    );
    let states = Arc::new(Mutex::new(Vec::new()));
    let messages = Arc::new(Mutex::new(Vec::new()));
    tee_states(
        from_net,
        to_view,
        Arc::clone(&states),
        Arc::clone(&messages),
    );
    (
        LiveClient {
            tx,
            states,
            messages,
        },
        view_events,
    )
}

impl Harness<'_> {
    /// Lets the view settle and asks `done` until it holds: a wait on the
    /// observation, polled with a real sleep because the daemon and the
    /// network thread run on real clocks the test executor never advances.
    /// Its only bound is [`HANG_GUARD`], the live and OS tiers' guard.
    pub fn wait_until(&mut self, what: &str, done: impl FnMut(&mut Self) -> bool) {
        self.wait_within(HANG_GUARD, what, done);
    }

    /// [`Self::wait_until`] for an in-process UI spec, whose loopback fakes
    /// answer at once: its only bound is [`IN_PROCESS_HANG_GUARD`].
    pub fn wait_until_in_process(&mut self, what: &str, done: impl FnMut(&mut Self) -> bool) {
        self.wait_within(IN_PROCESS_HANG_GUARD, what, done);
    }

    fn wait_within(
        &mut self,
        guard: Duration,
        what: &str,
        mut done: impl FnMut(&mut Self) -> bool,
    ) {
        let deadline = Instant::now() + guard;
        loop {
            self.cx.run_until_parked();
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "hung: waited {guard:?} for {what}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until a row of `pane` satisfies `row`, up to [`HANG_GUARD`]; a
    /// hang's failure shows what the pane holds.
    pub fn wait_for_row(&mut self, pane: &str, what: &str, row: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + HANG_GUARD;
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
                "hung: waited {HANG_GUARD:?} for {what}; pane {pane} shows {rows:#?}"
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
/// an identity kept beside it, a stop that kills only its pid, and the
/// tailnet dial's `tls` and device `tokens`.
fn live_deps(
    config: &Path,
    tls: Option<Arc<rustls::ClientConfig>>,
    tokens: Arc<MemoryTokens>,
) -> NetDeps {
    let identity = daemon_client::client_identity_in(config, CLIENT_ID_FILE)
        .expect("create the live client's identity");
    let ensure_dir = config.to_path_buf();
    let stop_dir = config.to_path_buf();
    NetDeps {
        ensure: Box::new(move || {
            let dir = ensure_dir.clone();
            Box::pin(async move { daemon_client::read_handshake_in(&dir) })
        }),
        // A real daemon answers the proof: the production open.
        open_local: Box::new(|port, token| {
            Box::pin(async move {
                daemon_client::connect_proven(port, &token, daemon_client::PROOF_TIMEOUT).await
            })
        }),
        // The production start: `RUSTLING_TULIP_SSH` names the binary, which
        // the SSH specs set to the fake ssh (see [`FakeSsh`]).
        open_ssh: Box::new(|host| {
            let host = host.clone();
            Box::pin(async move { rustling_tulip_native::ssh::open(&host) })
        }),
        identity: Some(identity),
        stop: Box::new(move || {
            let dir = stop_dir.clone();
            Box::pin(async move { daemon_client::stop_in(&dir).await })
        }),
        tokens,
        tls,
    }
}

/// Forwards every network event to the view, recording each state and each
/// daemon message on the way.
fn tee_states(
    mut from_net: futures::channel::mpsc::UnboundedReceiver<NetEvent>,
    to_view: UnboundedSender<NetEvent>,
    states: Arc<Mutex<Vec<Connection>>>,
    messages: Arc<Mutex<Vec<DaemonMessage>>>,
) {
    std::thread::spawn(move || {
        futures::executor::block_on(async move {
            while let Some(event) = from_net.next().await {
                match &event {
                    NetEvent::State(conn) => {
                        states.lock().expect("state log lock").push(conn.clone());
                    }
                    NetEvent::Message(msg) => {
                        let msg = DaemonMessage::clone(msg);
                        messages.lock().expect("message log lock").push(msg);
                    }
                    _ => {}
                }
                if to_view.unbounded_send(event).is_err() {
                    break;
                }
            }
        });
    });
}
