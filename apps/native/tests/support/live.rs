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

use agent_api::{AgentPlugin, FakeCli};
use daemon_client::device_tokens::MemoryTokens;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{Modifiers, TestAppContext};
use protocol::{AgentId, ClientMessage, DaemonHandshake, DaemonMessage};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{
    Connection, NetCommand, NetDeps, NetEvent, RootDeps, RootView, bind_keys, spawn_net,
};
use serde_json::{Value, json};

use super::{Harness, NotifyRecorder, OpenRecorder, Outbox, PickRecorder, TestClock};

/// The hang guard of every wait in the live and OS tiers: the daemon writing
/// `daemon.json` and answering `/health`, its processes leaving after a kill,
/// and every [`Harness::wait_until`] / [`Harness::wait_for_row`]. A passing run
/// never reaches it (load-proof-tests); reaching it fails as "hung".
pub const HANG_GUARD: Duration = Duration::from_secs(300);
/// The hang guard of [`Harness::wait_until_in_process`], for the in-process
/// UI specs: under nextest's 300 s kill, so a hang fails with its own
/// "hung" message rather than as TIMEOUT.
pub const IN_PROCESS_HANG_GUARD: Duration = Duration::from_secs(120);
/// The id file the native client keeps its identity in.
const CLIENT_ID_FILE: &str = "client-id-native";
const MISSING_DAEMON: &str = "rustling-tulipd.exe not found next to the test binary; \
                              run `.\\rt.ps1 native-e2e` or `.\\rt.ps1 native-smoke`; \
                              they build daemon + tracer first";

/// The repository root, for the `.tmp` dir and `tools/e2e/fake-tailscale`.
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

/// `rustling-tulipd` beside the test binary, checked to be there.
fn daemon_binary() -> PathBuf {
    let daemon = target_bin_dir().join(format!("rustling-tulipd{}", std::env::consts::EXE_SUFFIX));
    assert!(
        daemon.is_file(),
        "{MISSING_DAEMON} (looked for {})",
        daemon.display()
    );
    daemon
}

/// Starts `rustling-tulipd` from beside the test binary with `envs` set and no
/// console or standard streams.
fn spawn_daemon(envs: &[(&'static str, OsString)]) -> Child {
    let mut command = Command::new(daemon_binary());
    command
        .envs(envs.iter().map(|(key, value)| (*key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_console(&mut command);
    command.spawn().expect("spawn rustling-tulipd")
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
        Self::spawn_in(scratch_root(test), test, extra, None)
    }

    /// [`Self::start`] for remote access: the daemon's `tailscale` is
    /// `tools/e2e/fake-tailscale`, answering as the host [`REMOTE_HOST`]
    /// whose owner every peer is, and its `cert` hands out a leaf for
    /// [`REMOTE_HOST`] signed by a test CA that [`Self::tailnet_tls`] trusts.
    /// Remote access stays off until a client sends `ConfigureRemote`; the
    /// daemon needs the `e2e-loopback-peers` build to admit a loopback dial.
    pub fn start_remote(test: &str) -> Self {
        let root = scratch_root(test);
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
        Self::spawn_in(
            root,
            test,
            &[(FAKE_TAILSCALE_STATE_ENV, state_file.into_os_string())],
            Some(ca.der),
        )
    }

    /// Starts the daemon over `root`, made by [`scratch_root`], and waits for
    /// its handshake and `/health`.
    fn spawn_in(
        root: PathBuf,
        test: &str,
        extra: &[(&'static str, OsString)],
        tailnet_ca: Option<Vec<u8>>,
    ) -> Self {
        let mut envs = isolated_envs(&root, test);
        envs.extend(extra.iter().cloned());
        let child = spawn_daemon(&envs);
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
                build_version: String::new(),
            },
            tailnet_ca,
        };
        live.handshake = live.wait_healthy();
        live
    }

    fn wait_healthy(&mut self) -> DaemonHandshake {
        let deadline = Instant::now() + HANG_GUARD;
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
                "hung: rustling-tulipd wrote no healthy daemon.json within {HANG_GUARD:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Makes the fake tailscale's `whois` answer that every peer belongs to
    /// a user other than the host's owner, for a daemon from
    /// [`Self::start_remote`]. The shim reads its state on every run, so the
    /// next `whois` already answers so.
    pub fn make_peers_non_owners(&self) {
        let path = self.root.join("fake-tailscale").join("state.json");
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
        let path = self.config_dir().join("logs").join("daemon.log");
        match std::fs::read_to_string(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            read => read
                .map_err(|err| format!("{}: {err}", path.display()))
                .expect("reading the daemon log"),
        }
    }

    /// Whether the daemon still answers `GET /health` on its loopback port.
    pub fn answers_health(&self) -> bool {
        health_ok(self.handshake.port)
    }

    /// The test's scratch dir; fixtures go here.
    pub fn dir(&self) -> &Path {
        &self.root
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    /// The home folder `agent`'s fake CLI names in its `home_env`: where the
    /// daemon and the sessions it spawns look for that agent's files, in
    /// place of the user's own.
    pub fn agent_home(&self, agent: &AgentId) -> PathBuf {
        agent_home(&self.root, agent)
    }

    /// The daemon's binary cache: the tracers, and a daemon it respawns,
    /// run from copies here named `<name>-<hash>.exe`.
    pub fn binaries_dir(&self) -> PathBuf {
        self.root.join("binaries")
    }

    /// The daemon's `RUSTLING_TULIP_TAILNET_DIR`, where `tailscale cert`
    /// writes the listener's certificate.
    pub fn tailnet_dir(&self) -> PathBuf {
        self.root.join("tailnet")
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

    /// Kills the daemon and every tracer it started, as a reboot would, then
    /// starts a new daemon on the same directories.
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
        self.child = spawn_daemon(&self.envs);
        self.handshake = self.wait_healthy();
    }

    /// Stops the daemon the way a user would, with `rustling-tulipd shutdown`
    /// under its isolation variables, and waits for its process to exit.
    pub fn shut_down_by_cli(&mut self) {
        let mut command = Command::new(daemon_binary());
        command
            .arg("shutdown")
            .envs(self.envs.iter().map(|(key, value)| (*key, value)))
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
            .child
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
        let dir = self.root.join("fake-ssh");
        std::fs::create_dir_all(&dir).expect("create the fake ssh's dir");
        let env: serde_json::Map<String, Value> = self
            .envs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), json!(value.to_string_lossy())))
            .collect();
        let log = dir.join("argv.jsonl");
        let state = json!({
            "bridge": daemon_binary(),
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

/// What a `PATH` stub prints before its arguments.
pub const STUB_MARKER: &str = "[rt-e2e agent stub]";

/// A `program` in `dir` that prints [`STUB_MARKER`] and its arguments, so a
/// shell the daemon starts never runs the user's real CLI.
#[cfg(windows)]
fn write_path_stub(dir: &Path, program: &str) {
    let script = format!("@echo off\r\necho {STUB_MARKER} %*\r\n");
    std::fs::write(dir.join(format!("{program}.cmd")), script).expect("write the PATH stub");
}

#[cfg(not(windows))]
fn write_path_stub(dir: &Path, program: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(program);
    let script = format!("#!/bin/sh\necho '{STUB_MARKER}' \"$@\"\n");
    std::fs::write(&path, script).expect("write the PATH stub");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make the PATH stub executable");
}

/// `plugin`'s fake CLI; the live tier runs no agent without one.
#[expect(clippy::panic, reason = "the failure names the agent that has no fake")]
fn fake_cli(plugin: &dyn AgentPlugin) -> FakeCli {
    let id = &plugin.descriptor().id.0;
    plugin
        .fake_cli()
        .unwrap_or_else(|| panic!("agent {id} has no fake CLI for the live tier"))
}

/// `<root>/<agent id>-home`: the folder `agent`'s fake `home_env` names.
fn agent_home(root: &Path, agent: &AgentId) -> PathBuf {
    root.join(format!("{}-home", agent.0))
}

/// `PATH` with the stubs' dir first.
fn path_with_stub(root: &Path) -> OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(root.join("fake-bin")).chain(std::env::split_paths(&inherited));
    std::env::join_paths(dirs).expect("PATH entries hold no separator")
}

/// The `MagicDNS` name the fake tailscale gives the host, without its trailing
/// dot: the name its certificate is for and a tailnet client dials.
pub const REMOTE_HOST: &str = "localhost";
/// The owner's Tailscale user id in the fake tailnet.
const OWNER_USER_ID: i64 = 2;
/// The variable naming the fake tailscale's state file.
const FAKE_TAILSCALE_STATE_ENV: &str = "RUSTLING_TULIP_FAKE_TAILSCALE_STATE";

/// `<repo>/.tmp/native-e2e/<test>-<pid>/`, emptied, with the daemon's
/// directories, each registered agent's home folder and its `PATH` stub made.
fn scratch_root(test: &str) -> PathBuf {
    let root = repo_root()
        .join(".tmp")
        .join("native-e2e")
        .join(format!("{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for sub in [
        "config",
        "binaries",
        "worktrees",
        "tailnet",
        "ui",
        "fake-bin",
    ] {
        std::fs::create_dir_all(root.join(sub)).expect("create the test's scratch dirs");
    }
    for plugin in agent_registry::all() {
        let fake = fake_cli(*plugin);
        if fake.home_env.is_some() {
            std::fs::create_dir_all(agent_home(&root, &plugin.descriptor().id))
                .expect("create the agent's home folder");
        }
        if let Some(program) = fake.path_stub {
            write_path_stub(&root.join("fake-bin"), program);
        }
    }
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

/// A loopback port nothing listens on right now.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .expect("bind a free loopback port")
        .port()
}

fn isolated_envs(root: &Path, test: &str) -> Vec<(&'static str, OsString)> {
    let fake_tailscale = if cfg!(windows) {
        "fake-tailscale.cmd"
    } else {
        "fake-tailscale.sh"
    };
    let fake_tailscale = repo_root()
        .join("tools")
        .join("e2e")
        .join("fake-tailscale")
        .join(fake_tailscale);
    let mut envs = vec![
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
        // The tailnet listener's CLI and certificate dir: never the user's
        // Tailscale or their real certificate, even with remote access off.
        ("RUSTLING_TULIP_TAILSCALE", fake_tailscale.into_os_string()),
        (
            "RUSTLING_TULIP_TAILNET_DIR",
            root.join("tailnet").into_os_string(),
        ),
        // A shell the daemon starts finds the agents' stubs before any real
        // CLI.
        ("PATH", path_with_stub(root)),
        // The tracer the daemon copies into its binaries dir: the one built
        // beside it, never the installed app's.
        (
            "RUSTLING_TULIP_BIN_TEMPLATES",
            target_bin_dir().into_os_string(),
        ),
    ];
    // Every registered agent runs its fake, with its home under the test's
    // root: never the user's real CLI or its files.
    for plugin in agent_registry::all() {
        let fake = fake_cli(*plugin);
        envs.push((plugin.program_env_var(), fake.shim.into_os_string()));
        if let Some(home_env) = fake.home_env {
            let home = agent_home(root, &plugin.descriptor().id);
            envs.push((home_env, home.into_os_string()));
        }
    }
    envs
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

/// Force-kills `pid` and, on Windows, every process under it.
pub fn kill_tree(pid: u32) {
    kill(pid, true);
}

/// Force-kills `pid` alone, leaving the processes under it to whatever
/// owns them (a tracer's child dies with the tracer's kill-on-close job).
pub fn kill_process(pid: u32) {
    kill(pid, false);
}

/// Force-kills `pid`, with its tree when `tree` is set. A process that is
/// already gone is not an error.
#[cfg(windows)]
fn kill(pid: u32, tree: bool) {
    let pid = pid.to_string();
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid, "/F"]);
    if tree {
        command.arg("/T");
    }
    command.stdout(Stdio::null()).stderr(Stdio::null());
    no_console(&mut command);
    let _ = command.status();
}

/// Force-kills `pid`; `kill` has no tree walk, so `tree` changes nothing.
#[cfg(not(windows))]
fn kill(pid: u32, _tree: bool) {
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
        Self::open_live_on(
            cx,
            daemon,
            Target::Local,
            None,
            Arc::new(MemoryTokens::default()),
        )
    }

    /// [`Self::open_live`] on `target`: a tailnet dial trusts only `tls`'s
    /// roots (`None` for the OS store) and holds the device tokens in
    /// `tokens`, which the view's pairing flow shares.
    pub fn open_live_on(
        cx: &'a mut TestAppContext,
        daemon: &LiveDaemon,
        target: Target,
        tls: Option<Arc<rustls::ClientConfig>>,
        tokens: Arc<MemoryTokens>,
    ) -> (Self, LiveClient) {
        cx.update(bind_keys);
        let (tx, net_commands) = unbounded();
        let (net_events, from_net) = unbounded();
        let (to_view, view_events) = unbounded();
        spawn_net(
            live_deps(&daemon.config_dir(), tls, tokens.clone()),
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
        let clock = TestClock::new();
        let quits = std::rc::Rc::new(std::cell::Cell::new(0));
        let opener = Arc::new(OpenRecorder::default());
        let notifier = Arc::new(NotifyRecorder::default());
        let picker = std::rc::Rc::new(PickRecorder::default());
        let autostart = Arc::new(super::AutostartRecorder::default());
        let updater = Arc::new(super::UpdaterRecorder::default());
        let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let deps = RootDeps {
            open_host: super::fake_transport((tx.clone(), view_events), &opened),
            ui_dir: Some(daemon.dir().join("ui")),
            paths: Err("live specs have no log paths".to_owned()),
            wanted: None,
            now: clock.clock(),
            quit: super::quit_recorder(&quits),
            open: opener.clone(),
            notify: notifier.clone(),
            pick_folder: PickRecorder::picker(&picker),
            pick_file: PickRecorder::file_picker(&picker),
            pick_save: PickRecorder::save_picker(&picker),
            autostart: autostart.clone(),
            tokens: tokens.clone(),
            updater: updater.clone(),
        };
        let (root, cx) =
            cx.add_window_view(move |window, cx| RootView::with_transport(deps, window, cx));
        let harness = Self {
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
            extra_hosts: super::ExtraHosts::default(),
        };
        (
            harness,
            LiveClient {
                tx,
                states,
                messages,
            },
        )
    }

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
