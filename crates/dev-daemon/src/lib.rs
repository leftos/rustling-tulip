//! An isolated `rustling-tulipd`: its config, binaries, worktrees and tailnet
//! dirs and its tracer pipe prefix all live under one scratch folder under a
//! `.tmp` folder, set on the child process only, and every registered agent
//! runs its fake CLI. [`IsolatedDaemon::start`] refuses any other folder
//! before it spawns or makes anything; drop kills the daemon and the tracers
//! it started, then removes the folder.

mod env;
mod process;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use daemon_client::isolation::{NotIsolated, Reason, check_isolated};
use protocol::{AgentId, DaemonHandshake};

pub use env::STUB_MARKER;
pub use process::{free_port, kill_process, kill_tree, no_console, processes_under};

/// The hang guard of the wait for the daemon's `daemon.json` and `/health`.
/// A healthy start never reaches it; reaching it fails as "hung".
pub const HANG_GUARD: Duration = Duration::from_secs(300);

/// How many of `daemon.log`'s last lines a hung start reports.
const LOG_TAIL_LINES: usize = 20;

/// The shell an isolated daemon gives its shell sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TestShell {
    /// A shell whose prompt and echo are the same on every run:
    /// `RUSTLING_TULIP_SHELL` names `ComSpec`, and shell integration is off.
    #[default]
    Cmd,
    /// The daemon's Default shell setting, which a fresh config dir
    /// auto-detects, with shell integration on: neither `RUSTLING_TULIP_SHELL`
    /// nor `RUSTLING_TULIP_SHELL_INTEGRATION` is set.
    Default,
}

/// What to start: `name` goes into the tracer pipe prefix; `scratch_root`
/// is the folder every directory goes under, which must sit under a `.tmp`
/// folder; `repo_root` holds the `tools/e2e` fakes the daemon runs;
/// `bin_dir` holds `rustling-tulipd` and the tracer it copies; `extra_env`
/// is set on the daemon after the isolation variables, so a row can
/// override one; `shell` is the shell its sessions get, as [`TestShell`] says.
pub struct DaemonSpec {
    pub name: String,
    pub scratch_root: PathBuf,
    pub repo_root: PathBuf,
    pub bin_dir: PathBuf,
    pub extra_env: Vec<(&'static str, OsString)>,
    pub shell: TestShell,
}

/// The variables an isolated daemon must not inherit from its parent, so a
/// `Default` spec's shell really comes from the daemon's own setting.
#[must_use]
pub fn removed_envs(spec: &DaemonSpec) -> &'static [&'static str] {
    match spec.shell {
        TestShell::Cmd => &[],
        TestShell::Default => &["RUSTLING_TULIP_SHELL", "RUSTLING_TULIP_SHELL_INTEGRATION"],
    }
}

/// Why an isolated daemon did not start.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// A folder the daemon would write to is not under a `.tmp` folder;
    /// nothing was spawned or made.
    #[error("refusing to start an isolated daemon: {}", not_isolated(.variable, .reason))]
    NotIsolated {
        variable: &'static str,
        reason: Reason,
    },
    #[error(
        "rustling-tulipd.exe not found in {}; build daemon + tracer first \
         (`.\\rt.ps1 build`, or `.\\rt.ps1 native-e2e` / `native-smoke`)",
        bin_dir_of(.looked_for).display()
    )]
    MissingDaemon { looked_for: PathBuf },
    #[error("agent {agent} has no fake CLI for an isolated daemon")]
    NoFakeCli { agent: String },
    #[error("cannot put the agent stubs' folder first on PATH: {0}")]
    StubPath(#[from] std::env::JoinPathsError),
    #[error("could not make {}: {source}", .path.display())]
    Scratch {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("spawn rustling-tulipd: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("rustling-tulipd exited during startup ({status}); see {}", .log.display())]
    Exited { status: ExitStatus, log: PathBuf },
    #[error(
        "hung: rustling-tulipd wrote no healthy daemon.json within {HANG_GUARD:?}; daemon.log ends:\n{log_tail}"
    )]
    NotHealthy { log_tail: String },
}

/// The check's own wording of a refusal, for [`LaunchError::NotIsolated`].
fn not_isolated(variable: &&'static str, reason: &Reason) -> NotIsolated {
    NotIsolated {
        variable,
        reason: reason.clone(),
    }
}

/// The folder a missing daemon was looked for in.
fn bin_dir_of(looked_for: &Path) -> &Path {
    looked_for.parent().unwrap_or(looked_for)
}

impl From<NotIsolated> for LaunchError {
    fn from(err: NotIsolated) -> Self {
        Self::NotIsolated {
            variable: err.variable,
            reason: err.reason,
        }
    }
}

/// A running `rustling-tulipd` with every directory under its scratch root.
/// Drop kills it and the tracers it spawned, then removes the root, unless
/// the thread is panicking or the start failed: the daemon's log stays
/// under `config/logs/`.
pub struct IsolatedDaemon {
    root: PathBuf,
    daemon: PathBuf,
    child: Child,
    envs: Vec<(&'static str, OsString)>,
    removed_envs: &'static [&'static str],
    handshake: DaemonHandshake,
    keep_root: bool,
}

impl IsolatedDaemon {
    /// Builds the isolated environment, refuses it unless each of its
    /// folders and the scratch root sit under a `.tmp` folder, then makes
    /// the folders, starts the daemon from `spec.bin_dir` and waits for its
    /// handshake and `/health`.
    pub fn start(spec: DaemonSpec) -> Result<Self, LaunchError> {
        let envs = env::isolated_envs(&spec)?;
        check_isolated(|name| env::env_value(&envs, name))?;
        check_scratch_root(&spec.scratch_root)?;
        let daemon = spec
            .bin_dir
            .join(format!("rustling-tulipd{}", std::env::consts::EXE_SUFFIX));
        if !daemon.is_file() {
            return Err(LaunchError::MissingDaemon { looked_for: daemon });
        }
        env::make_scratch_dirs(&spec.scratch_root)?;
        let removed = removed_envs(&spec);
        let child = process::spawn_daemon(&daemon, &envs, removed).map_err(LaunchError::Spawn)?;
        let mut isolated = Self {
            root: spec.scratch_root,
            daemon,
            child,
            envs,
            removed_envs: removed,
            handshake: DaemonHandshake {
                protocol_version: 0,
                port: 0,
                auth_token: String::new(),
                pid: 0,
                supported_versions: Vec::new(),
                build_version: String::new(),
            },
            keep_root: false,
        };
        isolated.wait_healthy()?;
        Ok(isolated)
    }

    /// Starts a new daemon on the same directories and environment, after
    /// [`Self::kill`], and waits for its handshake and `/health`.
    pub fn respawn(&mut self) -> Result<(), LaunchError> {
        self.child =
            process::spawn_daemon(&self.daemon, &self.envs, self.removed_envs).map_err(|err| {
                self.keep_root = true;
                LaunchError::Spawn(err)
            })?;
        self.wait_healthy()
    }

    /// Waits for a `daemon.json` naming this child whose port answers
    /// `/health`, and keeps its handshake; a failure keeps the root.
    fn wait_healthy(&mut self) -> Result<(), LaunchError> {
        let deadline = Instant::now() + HANG_GUARD;
        let config = self.config_dir();
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.keep_root = true;
                return Err(LaunchError::Exited {
                    status,
                    log: self.daemon_log_path(),
                });
            }
            if let Ok(handshake) = daemon_client::read_handshake_in(&config)
                && handshake.pid == self.child.id()
                && process::health_ok(handshake.port)
            {
                self.handshake = handshake;
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.keep_root = true;
                return Err(LaunchError::NotHealthy {
                    log_tail: self.log_tail(),
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `daemon.log`'s last lines, or why it cannot be read.
    fn log_tail(&self) -> String {
        let path = self.daemon_log_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n")
            }
            Err(err) => format!("({}: {err})", path.display()),
        }
    }

    fn daemon_log_path(&self) -> PathBuf {
        self.config_dir().join("logs").join("daemon.log")
    }

    /// The daemon's log as written so far; empty before the daemon made the
    /// file, which a poll for a line reads as "not logged yet".
    pub fn daemon_log(&self) -> std::io::Result<String> {
        match std::fs::read_to_string(self.daemon_log_path()) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            read => read,
        }
    }

    /// Whether the daemon still answers `GET /health` on its loopback port.
    #[must_use]
    pub fn answers_health(&self) -> bool {
        process::health_ok(self.handshake.port)
    }

    /// The scratch root; fixtures go here.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    /// The home folder `agent`'s fake CLI names in its `home_env`: where the
    /// daemon and the sessions it spawns look for that agent's files, in
    /// place of the user's own.
    #[must_use]
    pub fn agent_home(&self, agent: &AgentId) -> PathBuf {
        env::agent_home(&self.root, agent)
    }

    /// The daemon's binary cache: the tracers, and a daemon it respawns,
    /// run from copies here named `<name>-<hash>.exe`.
    #[must_use]
    pub fn binaries_dir(&self) -> PathBuf {
        self.root.join("binaries")
    }

    /// The daemon's `RUSTLING_TULIP_TAILNET_DIR`, where `tailscale cert`
    /// writes the listener's certificate.
    #[must_use]
    pub fn tailnet_dir(&self) -> PathBuf {
        self.root.join("tailnet")
    }

    /// The variables that isolate a process to this daemon, to set on a
    /// child with `Command::envs`.
    #[must_use]
    pub fn envs(&self) -> &[(&'static str, OsString)] {
        &self.envs
    }

    #[must_use]
    pub fn handshake(&self) -> &DaemonHandshake {
        &self.handshake
    }

    /// The `rustling-tulipd` this daemon runs, in the spec's `bin_dir`.
    #[must_use]
    pub fn daemon_exe(&self) -> &Path {
        &self.daemon
    }

    /// The daemon's process, as last started.
    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Force-kills the daemon alone, leaving its tracers running, as a crash
    /// would, and reaps it.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()?;
        self.child.wait()?;
        Ok(())
    }
}

/// Refuses a scratch root that is not itself under a `.tmp` folder, which
/// drop would otherwise remove whole.
fn check_scratch_root(root: &Path) -> Result<(), LaunchError> {
    check_isolated(|_| Some(root.as_os_str().to_owned())).map_err(|err| LaunchError::NotIsolated {
        variable: "the scratch root",
        reason: err.reason,
    })
}

impl Drop for IsolatedDaemon {
    fn drop(&mut self) {
        let config = self.config_dir();
        let tracers = process::sidecar_tracer_pids(&config);
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
        for pid in pids.filter(|pid| process::image_under(*pid, &binaries)) {
            kill_tree(pid);
        }
        // A failed start or a failed test keeps the root: the daemon and
        // client logs are under `config/logs/`.
        if !self.keep_root && !std::thread::panicking() {
            process::remove_dir_retrying(&self.root);
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and panic; failure messages aid debugging"
)]
mod tests {
    use daemon_client::isolation::ISOLATED_DIR_VARS;

    use super::*;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crates/dev-daemon sits two levels under the repo root")
            .to_path_buf()
    }

    fn spec(scratch_root: PathBuf) -> DaemonSpec {
        DaemonSpec {
            name: "unit".to_owned(),
            scratch_root,
            repo_root: repo_root(),
            bin_dir: repo_root().join("target").join("no-such-profile"),
            extra_env: Vec::new(),
            shell: TestShell::Cmd,
        }
    }

    #[test]
    fn launch_refuses_without_isolated_dirs() {
        let scratch =
            std::env::temp_dir().join(format!("rt-dev-daemon-refused-{}", std::process::id()));
        assert!(
            !scratch.exists(),
            "precondition: {} does not exist yet",
            scratch.display()
        );
        match IsolatedDaemon::start(spec(scratch.clone())) {
            Err(LaunchError::NotIsolated { .. }) => {}
            Err(other) => panic!("expected NotIsolated, got {other}"),
            Ok(_) => panic!("a daemon started under {}", scratch.display()),
        }
        assert!(
            !scratch.exists(),
            "the refused launch created {}",
            scratch.display()
        );
    }

    #[test]
    fn isolated_envs_pass_check_isolated() {
        let scratch = repo_root()
            .join(".tmp")
            .join("dev-daemon-unit")
            .join(format!("envs-{}", std::process::id()));
        let envs = env::isolated_envs(&spec(scratch.clone()))
            .expect("every registered agent has a fake CLI");
        let get = |name: &str| env::env_value(&envs, name);
        if let Err(err) = check_isolated(get) {
            panic!("the built environment is not isolated: {err}");
        }
        for variable in ISOLATED_DIR_VARS {
            let value = PathBuf::from(get(variable).expect("check_isolated saw it set"));
            assert!(
                value.starts_with(&scratch),
                "{variable} = {} is not under {}",
                value.display(),
                scratch.display()
            );
        }
    }

    /// Every registered agent's fake is looked up under the repo root the
    /// launch names, so an installed driver runs it from that checkout.
    #[test]
    fn an_agents_fake_runs_from_the_launch_repo_root() {
        let scratch = repo_root()
            .join(".tmp")
            .join("dev-daemon-unit")
            .join(format!("fake-root-{}", std::process::id()));
        let other_checkout = scratch.join("other-checkout");
        let mut spec = spec(scratch);
        spec.repo_root = other_checkout.clone();
        let envs = env::isolated_envs(&spec).expect("every registered agent has a fake CLI");
        for plugin in agent_registry::all() {
            let id = &plugin.descriptor().id;
            let value = PathBuf::from(
                env::env_value(&envs, plugin.program_env_var())
                    .expect("every registered agent's program variable is set"),
            );
            let fake = plugin
                .fake_cli()
                .expect("every registered agent has a fake CLI");
            assert_eq!(
                value,
                other_checkout.join(&fake.shim_in_repo),
                "agent {id:?}: fake CLI is not the launch checkout's"
            );
        }
    }

    /// A spec that leaves the shell to the daemon's Default shell setting sets
    /// neither override, so a fresh config dir auto-detects with integration on.
    #[test]
    fn a_default_shell_spec_sets_no_shell_variables() {
        let scratch = repo_root()
            .join(".tmp")
            .join("dev-daemon-unit")
            .join(format!("shell-default-{}", std::process::id()));
        let mut spec = spec(scratch);
        spec.shell = TestShell::Default;
        let envs = env::isolated_envs(&spec).expect("every registered agent has a fake CLI");
        assert_eq!(env::env_value(&envs, "RUSTLING_TULIP_SHELL"), None);
        assert_eq!(
            env::env_value(&envs, "RUSTLING_TULIP_SHELL_INTEGRATION"),
            None
        );
    }

    /// Both names leave a `Default` spec's daemon, so an inherited
    /// `RUSTLING_TULIP_SHELL=cmd.exe` cannot decide its shell.
    #[test]
    fn a_default_shell_spec_removes_inherited_shell_variables() {
        let scratch = |name: &str| {
            repo_root()
                .join(".tmp")
                .join("dev-daemon-unit")
                .join(format!("{name}-{}", std::process::id()))
        };
        let mut default = spec(scratch("removed-default"));
        default.shell = TestShell::Default;
        assert_eq!(
            removed_envs(&default).to_vec(),
            vec!["RUSTLING_TULIP_SHELL", "RUSTLING_TULIP_SHELL_INTEGRATION"]
        );
        let cmd = spec(scratch("removed-cmd"));
        assert!(
            removed_envs(&cmd).is_empty(),
            "a Cmd spec keeps the inherited variables"
        );
    }

    /// A `Cmd` spec keeps the fixed prompt and echo the live specs match.
    #[test]
    fn a_cmd_shell_spec_keeps_cmd_and_no_integration() {
        let scratch = repo_root()
            .join(".tmp")
            .join("dev-daemon-unit")
            .join(format!("shell-cmd-{}", std::process::id()));
        let mut spec = spec(scratch);
        spec.shell = TestShell::Cmd;
        let envs = env::isolated_envs(&spec).expect("every registered agent has a fake CLI");
        let shell =
            env::env_value(&envs, "RUSTLING_TULIP_SHELL").expect("a cmd spec sets the shell");
        assert_eq!(
            shell,
            std::env::var_os("ComSpec").unwrap_or_else(|| OsString::from("cmd.exe"))
        );
        assert_eq!(
            env::env_value(&envs, "RUSTLING_TULIP_SHELL_INTEGRATION"),
            Some(OsString::from("0"))
        );
    }
}
