//! `launch`: shadow-copy a driver build, start an isolated daemon from the
//! copy, start the driven client against it, and wait until its endpoint
//! answers.
//!
//! The wait for the client's discovery file counts load-adjusted time on the
//! process's one clock (`pipe::clock`), with a 5x wall backstop, and ends
//! sooner when the client stalls: neither its log nor its CPU grows for
//! [`DEFAULT_STALL_LIMIT`] of wall time.

use std::ffi::OsString;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use daemon_client::isolation::REMOTE_FILES_DIR_VAR;
use dev_daemon::{DaemonSpec, IsolatedDaemon, TestShell};
use load_clock::{DEFAULT_STALL_LIMIT, Deadline, Expiry, StallWatch, SystemTicker};
use serde::Deserialize;
use serde_json::json;

use crate::pipe::{PipeClient, Transport as _, clock};
use crate::session::{RemoteHost, ScratchRepo, Session, log_tail, native_log};
use crate::token;

/// What a missing driver build is told to run.
pub const BUILD_HINT: &str = "build the driven client first: cargo build -p rustling-tulip-native --features driver \
     --profile test-release, then cargo build -p daemon -p tracer --profile test-release (through tools/gate.ps1)";

/// The exes a session runs, shadow-copied out of the build folder.
const EXES: [&str; 3] = ["rustling-tulip-native", "rustling-tulipd", "rt-tracer"];

/// The hang guard of the wait for the client's discovery file, in
/// load-adjusted time with a 5x wall backstop. A healthy start writes it
/// after its first cloaked frame, in seconds.
const DISCOVERY_GUARD: Duration = Duration::from_secs(60);

/// The folder under the temp dir with one discovery file per driven client.
const DISCOVERY_FOLDER: &str = "rustling-tulip-driver";

/// Set to `1`, the client opens its driver endpoint.
const DRIVER_ENV: &str = "RUSTLING_TULIP_DRIVER";

/// Set, the client's window opens cloaked and never activates.
const OFFSCREEN_ENV: &str = "RUSTLING_TULIP_OFFSCREEN_WINDOW";

/// The `ssh` binary the client runs for an SSH host; set only on a remote
/// host's client, to the checkout's fake.
const SSH_ENV: &str = "RUSTLING_TULIP_SSH";

/// The fake ssh's state file; set only on a remote host's client.
const FAKE_SSH_STATE_ENV: &str = "RUSTLING_TULIP_FAKE_SSH_STATE";

/// The SSH host name the client's saved host carries; the fake ssh ignores
/// it and reaches the one remote daemon.
pub const REMOTE_HOST: &str = "devbox";

/// The client's layout file, which a remote host's run seeds with the saved
/// host.
const UI_FILE: &str = "native-ui.json";

/// The file a real Claude's home is seeded with, so its first start skips
/// onboarding.
pub const SEED_FILE: &str = ".claude.json";

/// What to launch: `scratch` is the session's own folder under the repo's
/// `.tmp/driver/`; `env` is set on the isolated daemon after its isolation
/// variables, so a row can point an agent or the tailnet at a real CLI;
/// `daemon_only_env` is set on the daemon alone, never on the client;
/// `remote_host` also starts a second isolated daemon the client reaches as
/// the saved SSH host [`REMOTE_HOST`] through the fake ssh.
pub struct LaunchSpec {
    pub session: String,
    pub repo_root: PathBuf,
    pub build_dir: PathBuf,
    pub scratch: PathBuf,
    pub shell: TestShell,
    pub env: Vec<(&'static str, OsString)>,
    pub daemon_only_env: Vec<(&'static str, OsString)>,
    pub remote_host: bool,
}

/// The client's two fake ssh variables: the checkout's shim and the state
/// file naming the remote daemon.
struct ClientSsh {
    shim: PathBuf,
    state: PathBuf,
}

/// What the client's command is built from: `envs` is the home daemon's
/// [`IsolatedDaemon::envs`], `removed` the variables it must not inherit,
/// and `ssh` the fake ssh's rows for a remote host's run.
struct ClientSpec<'a> {
    envs: &'a [(&'static str, OsString)],
    bin: &'a Path,
    scratch: &'a Path,
    removed: &'a [&'static str],
    ssh: Option<&'a ClientSsh>,
}

/// A driven client's discovery file, as the endpoint writes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Discovery {
    pub pid: u32,
    pub pipe: String,
    pub started_at: u64,
    pub version: String,
}

/// Launches `spec`'s session. A failure names its step, kills whatever was
/// started, and carries the client's `native.log` tail when there is one.
pub fn launch(spec: &LaunchSpec) -> Result<Session, String> {
    check_build(&spec.build_dir)?;
    let shim = if spec.remote_host {
        Some(fake_ssh_shim(&spec.repo_root)?)
    } else {
        None
    };
    // Dropped on any failure below, which removes it again.
    let scratch_repo = ScratchRepo::create(&spec.repo_root, &spec.session)?;
    let bin = spec.scratch.join("bin");
    shadow_copy(&spec.build_dir, &bin).map_err(|err| format!("shadow-copying the build: {err}"))?;
    let daemon_spec = daemon_spec(spec, &bin);
    let removed = dev_daemon::removed_envs(&daemon_spec);
    let daemon = IsolatedDaemon::start(daemon_spec)
        .map_err(|err| format!("starting the isolated daemon: {err}"))?;
    if token::uses_real_agent(&spec.env) {
        seed_real_agent_home(&daemon)?;
    }
    // Dropped before the home daemon on any failure below: locals drop in
    // reverse order.
    let remote = match shim {
        Some(shim) => Some(start_remote(spec, &bin, shim, &daemon.config_dir())?),
        None => None,
    };
    let log = native_log(&daemon.config_dir());
    let fail = |step: &str, err: &str| {
        format!(
            "{step}: {err}\n\n--- {} ends ---\n{}",
            log.display(),
            log_tail(&log)
        )
    };
    let client_spec = ClientSpec {
        envs: daemon.envs(),
        bin: &bin,
        scratch: &spec.scratch,
        removed,
        ssh: remote.as_ref().map(|(_, ssh)| ssh),
    };
    let mut client = spawn_client(&client_spec).map_err(|err| fail("starting the client", &err))?;
    let remote = remote.map(|(remote, _)| remote);
    let pipe = match wait_discovery(&mut client, &spec.scratch, &log) {
        Ok(discovery) => format!(r"\\.\pipe\{}", discovery.pipe),
        Err(err) => {
            let message = fail("waiting for the driver endpoint", &err);
            drop(
                Session::new(
                    spec.session.clone(),
                    client,
                    PipeClient::new(String::new()),
                    daemon,
                    spec.repo_root.clone(),
                    scratch_repo,
                )
                .with_remote(remote),
            );
            return Err(message);
        }
    };
    let mut session = Session::new(
        spec.session.clone(),
        client,
        PipeClient::new(pipe),
        daemon,
        spec.repo_root.clone(),
        scratch_repo,
    )
    .with_remote(remote);
    if let Err(err) = session.pipe().call("ping", json!({})) {
        let message = fail("pinging the driver endpoint", &err.to_string());
        drop(session);
        return Err(message);
    }
    Ok(session)
}

/// The isolated daemon `spec` runs, from the copies in `bin`.
fn daemon_spec(spec: &LaunchSpec, bin: &Path) -> DaemonSpec {
    DaemonSpec {
        name: spec.session.clone(),
        scratch_root: spec.scratch.clone(),
        repo_root: spec.repo_root.clone(),
        bin_dir: bin.to_path_buf(),
        extra_env: spec.env.clone(),
        daemon_only_env: spec.daemon_only_env.clone(),
        shell: spec.shell,
    }
}

/// The remote host's daemon: the home one's checkout, copies and shell,
/// under `<scratch>/remote` with its own name, and none of the spec's rows,
/// so it runs the fake agents and never holds the driver token.
fn remote_daemon_spec(spec: &LaunchSpec, bin: &Path) -> DaemonSpec {
    DaemonSpec {
        name: format!("{}-remote", spec.session),
        scratch_root: spec.scratch.join("remote"),
        repo_root: spec.repo_root.clone(),
        bin_dir: bin.to_path_buf(),
        extra_env: Vec::new(),
        daemon_only_env: Vec::new(),
        shell: spec.shell,
    }
}

/// Starts the remote host's daemon, writes the fake ssh's state naming it
/// under `<scratch>/fake-ssh`, and seeds the client's layout in `ui_dir`
/// with the saved host; returns the remote host and the client's rows.
fn start_remote(
    spec: &LaunchSpec,
    bin: &Path,
    shim: PathBuf,
    ui_dir: &Path,
) -> Result<(RemoteHost, ClientSsh), String> {
    let daemon = IsolatedDaemon::start(remote_daemon_spec(spec, bin))
        .map_err(|err| format!("starting the remote host's isolated daemon: {err}"))?;
    let state = write_fake_ssh(
        &spec.scratch.join("fake-ssh"),
        daemon.daemon_exe(),
        daemon.envs(),
    )?;
    let remote = RemoteHost::new(daemon, state.clone());
    seed_saved_host(ui_dir)?;
    Ok((remote, ClientSsh { shim, state }))
}

/// The checkout's fake ssh shim, refusing a checkout without it and a
/// machine without `node` on `PATH`, which the shim runs under.
fn fake_ssh_shim(repo_root: &Path) -> Result<PathBuf, String> {
    let name = if cfg!(windows) {
        "fake-ssh.cmd"
    } else {
        "fake-ssh.sh"
    };
    let shim = repo_root
        .join("tools")
        .join("e2e")
        .join("fake-ssh")
        .join(name);
    if !shim.is_file() {
        return Err(format!(
            "remote_host needs the fake ssh, and {} is missing",
            shim.display()
        ));
    }
    let node = exe("node");
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(&node).is_file()));
    if !on_path {
        return Err(format!(
            "remote_host needs node on PATH to run the fake ssh {}",
            shim.display()
        ));
    }
    Ok(shim)
}

/// Writes the fake ssh's `state.json` in `dir`: `bridge` runs as the remote
/// command with `envs` over its own environment, no login is refused, and
/// each run is logged to `argv.jsonl` beside it. Returns the state file.
fn write_fake_ssh(
    dir: &Path,
    bridge: &Path,
    envs: &[(&'static str, OsString)],
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("making {}: {err}", dir.display()))?;
    let env: serde_json::Map<String, serde_json::Value> = envs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), json!(value.to_string_lossy())))
        .collect();
    let state = json!({
        "bridge": bridge,
        "env": env,
        "fail": null,
        "log": dir.join("argv.jsonl"),
    });
    let path = dir.join("state.json");
    std::fs::write(&path, state.to_string())
        .map_err(|err| format!("writing {}: {err}", path.display()))?;
    Ok(path)
}

/// Writes `<ui_dir>/native-ui.json` holding only the saved SSH host
/// [`REMOTE_HOST`], which the client connects at start; a file already
/// there is left as it is.
fn seed_saved_host(ui_dir: &Path) -> Result<(), String> {
    let seed = json!({"saved_hosts": [{"kind": "ssh", "name": REMOTE_HOST}]});
    write_new(&ui_dir.join(UI_FILE), &seed)
}

/// Seeds the home of the agent [`token::REAL_AGENT_ENV`] points at a real
/// CLI, found as the value of its fake's home variable in `daemon`'s
/// environment.
fn seed_real_agent_home(daemon: &IsolatedDaemon) -> Result<(), String> {
    let home = dev_daemon::agent_home_var(token::REAL_AGENT_ENV).and_then(|home_env| {
        daemon
            .envs()
            .iter()
            .rev()
            .find(|(name, _)| *name == home_env)
            .map(|(_, value)| PathBuf::from(value))
    });
    let Some(home) = home else {
        return Err(format!(
            "seeding the agent home: no registered agent's fake names a home for {}",
            token::REAL_AGENT_ENV
        ));
    };
    seed_onboarding(&home)
}

/// Writes `<home>/.claude.json` holding only `hasCompletedOnboarding` and
/// `theme`, so the first start skips onboarding; a file already there is
/// left as it is.
fn seed_onboarding(home: &Path) -> Result<(), String> {
    let seed = json!({"hasCompletedOnboarding": true, "theme": "dark"});
    write_new(&home.join(SEED_FILE), &seed)
}

/// Writes `seed` to `path` unless a file is already there, which is left
/// as it is.
fn write_new(path: &Path, seed: &serde_json::Value) -> Result<(), String> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);
    let mut file = match file {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(err) => return Err(format!("seeding {}: {err}", path.display())),
    };
    file.write_all(seed.to_string().as_bytes())
        .map_err(|err| format!("seeding {}: {err}", path.display()))
}

/// The exe file name of `stem` on this platform.
fn exe(stem: &str) -> String {
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

/// Refuses a build folder missing any exe a session runs.
fn check_build(build_dir: &Path) -> Result<(), String> {
    let missing: Vec<String> = EXES
        .iter()
        .map(|stem| exe(stem))
        .filter(|name| !build_dir.join(name).is_file())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} has no {}; {BUILD_HINT}",
        build_dir.display(),
        missing.join(", ")
    ))
}

/// Copies each exe into `bin`, so a rebuild never meets a running file.
fn shadow_copy(build_dir: &Path, bin: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(bin)?;
    for stem in EXES {
        let name = exe(stem);
        std::fs::copy(build_dir.join(&name), bin.join(&name))?;
    }
    Ok(())
}

/// Starts the copied client with the daemon's isolation and the driver
/// switched on, its stderr in `<scratch>/client-stderr.log`.
fn spawn_client(client: &ClientSpec<'_>) -> Result<Child, String> {
    let stderr_path = client.scratch.join("client-stderr.log");
    let stderr = File::create(&stderr_path)
        .map_err(|err| format!("creating {}: {err}", stderr_path.display()))?;
    client_command(client, Stdio::from(stderr))
        .spawn()
        .map_err(|err| format!("spawning the client: {err}"))
}

/// The copied client's command: `envs` (the daemon's [`IsolatedDaemon::envs`],
/// never its daemon-only rows) and the driver switched on, run in `scratch`.
/// The fake ssh's rows are set from `ssh`, and removed without one, so a
/// value the server inherited never reaches the client.
fn client_command(client: &ClientSpec<'_>, stderr: Stdio) -> Command {
    let mut command = Command::new(client.bin.join(exe("rustling-tulip-native")));
    // The client hands its environment on to any daemon it starts itself.
    for name in client.removed {
        command.env_remove(*name);
    }
    command.envs(client.envs.iter().map(|(key, value)| (*key, value)));
    match client.ssh {
        Some(ssh) => command
            .env(SSH_ENV, &ssh.shim)
            .env(FAKE_SSH_STATE_ENV, &ssh.state),
        None => command.env_remove(SSH_ENV).env_remove(FAKE_SSH_STATE_ENV),
    };
    command
        .env(DRIVER_ENV, "1")
        .env(OFFSCREEN_ENV, "1")
        .env(REMOTE_FILES_DIR_VAR, client.scratch.join("remote-files"))
        .current_dir(client.scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    dev_daemon::no_console(&mut command);
    command
}

/// Waits for the discovery file of `client`, matched by pid and creation
/// time, failing at once when the client exits, and when neither `log` nor
/// the client's CPU grows for [`DEFAULT_STALL_LIMIT`].
fn wait_discovery(client: &mut Child, scratch: &Path, log: &Path) -> Result<Discovery, String> {
    let started_at = process_times(client)?.created;
    let dir = std::env::temp_dir().join(DISCOVERY_FOLDER);
    let pid = client.id();
    let deadline = Deadline::start(clock(), DISCOVERY_GUARD);
    let mut stall = StallWatch::new(Arc::new(SystemTicker), DEFAULT_STALL_LIMIT);
    let observe = || Observed {
        exited: match client.try_wait() {
            Ok(Some(status)) => Some(format!(
                "the client exited ({status}); its stderr ends:\n{}",
                log_tail(&scratch.join("client-stderr.log"))
            )),
            _ => None,
        },
        found: find_discovery(&dir, pid, started_at),
        log_len: std::fs::metadata(log).map_or(0, |meta| meta.len()),
        cpu: process_times(client).map_or(Duration::ZERO, |times| times.cpu),
    };
    let pause = || std::thread::sleep(Duration::from_millis(100));
    let target = discovery_target(pid, &dir);
    discovery_loop(&deadline, &mut stall, observe, pause, &target)
}

/// One look at a starting client.
struct Observed {
    /// Why the client is gone, when it has exited.
    exited: Option<String>,
    /// Its discovery file, once written.
    found: Option<Discovery>,
    /// Its log's length in bytes.
    log_len: u64,
    /// Its kernel + user CPU so far.
    cpu: Duration,
}

/// The discovery wait's loop: each pass observes the client and fails on an
/// exit, ends on its file, then fails once `deadline` expires or `stall`
/// fires, else pauses. `target` names the file waited for
/// ([`discovery_target`]).
fn discovery_loop(
    deadline: &Deadline,
    stall: &mut StallWatch,
    mut observe: impl FnMut() -> Observed,
    mut pause: impl FnMut(),
    target: &str,
) -> Result<Discovery, String> {
    loop {
        let seen = observe();
        if let Some(exited) = seen.exited {
            return Err(exited);
        }
        if let Some(discovery) = seen.found {
            return Ok(discovery);
        }
        stall.observe(seen.log_len, seen.cpu);
        if let Some(expiry) = deadline.expired() {
            return Err(no_discovery(target, &expiry));
        }
        if let Some(expiry) = stall.stalled() {
            return Err(stalled(&expiry));
        }
        pause();
    }
}

/// The discovery file waited for, as the timeout error names it.
pub(crate) fn discovery_target(pid: u32, dir: &Path) -> String {
    format!("pid {pid} in {}", dir.display())
}

/// The error of a discovery file that did not come.
pub(crate) fn no_discovery(target: &str, expiry: &Expiry) -> String {
    format!("hung: no discovery file for {target}: {expiry}")
}

/// The error of a client that stalled while starting.
pub(crate) fn stalled(expiry: &Expiry) -> String {
    format!("the client {expiry}")
}

/// The discovery file in `dir` for process `pid` started at `started_at`;
/// a file whose start time differs belongs to an earlier process that had
/// the same pid.
#[must_use]
pub fn find_discovery(dir: &Path, pid: u32, started_at: u64) -> Option<Discovery> {
    let text = std::fs::read(dir.join(format!("{pid}.json"))).ok()?;
    let discovery: Discovery = serde_json::from_slice(&text).ok()?;
    (discovery.pid == pid && discovery.started_at == started_at).then_some(discovery)
}

/// A process's times, read through its handle.
struct ProcessTimes {
    /// Its creation time as a `FILETIME` in one u64.
    created: u64,
    /// Its kernel + user CPU so far.
    cpu: Duration,
}

/// `child`'s creation time and CPU, read through its handle.
#[cfg(windows)]
fn process_times(child: &Child) -> Result<ProcessTimes, String> {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::{FILETIME, HANDLE};
    use windows::Win32::System::Threading::GetProcessTimes;

    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the child's own process handle, alive while `child` is, and
    // out-pointers to locals.
    unsafe {
        GetProcessTimes(
            HANDLE(child.as_raw_handle()),
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .map_err(|err| format!("reading the client's start time: {err}"))?;
    let units =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let cpu = units(kernel).saturating_add(units(user));
    Ok(ProcessTimes {
        created: units(created),
        // 100 ns units.
        cpu: Duration::from_nanos(cpu.saturating_mul(100)),
    })
}

#[cfg(not(windows))]
fn process_times(_child: &Child) -> Result<ProcessTimes, String> {
    Err("a driven client runs on Windows only".to_owned())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the step it lost"
)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;

    use serde_json::json;

    use super::{
        BUILD_HINT, ClientSpec, ClientSsh, Discovery, FAKE_SSH_STATE_ENV, LaunchSpec, SEED_FILE,
        SSH_ENV, TestShell, UI_FILE, client_command, daemon_spec, find_discovery, launch,
        remote_daemon_spec, seed_onboarding, seed_saved_host, write_fake_ssh,
    };
    use crate::token::TOKEN_ENV;

    /// A spec for session `unit` under a scratch named `tag`, built from the
    /// checkout and never started.
    fn unit_spec(tag: &str, remote_host: bool) -> LaunchSpec {
        let root = repo_root();
        LaunchSpec {
            session: "unit".to_owned(),
            repo_root: root.clone(),
            build_dir: root.join("no-such-build"),
            scratch: root
                .join(".tmp")
                .join("driver")
                .join(format!("unit-{tag}-{}", std::process::id())),
            shell: TestShell::Cmd,
            env: Vec::new(),
            daemon_only_env: Vec::new(),
            remote_host,
        }
    }

    /// Whether `rows` name either of the fake ssh's variables.
    fn names_ssh(rows: &[(&'static str, OsString)]) -> bool {
        rows.iter()
            .any(|(name, _)| *name == SSH_ENV || *name == FAKE_SSH_STATE_ENV)
    }

    /// The SSH rows go on the client's command alone: neither daemon's
    /// environment carries them, the remote daemon gets none of the home
    /// spec's rows, and without a remote host the client's command removes
    /// both, so a value the server inherited never reaches it.
    #[test]
    fn the_ssh_rows_reach_the_client_only() {
        const SENTINEL: &str = "sentinel-driver-token-7a1c";
        let mut spec = unit_spec("ssh", true);
        spec.daemon_only_env = vec![(TOKEN_ENV, OsString::from(SENTINEL))];
        spec.env = vec![("RUSTLING_TULIP_PWA_DIR", OsString::from("C:\\tmp\\pwa"))];
        let bin = spec.scratch.join("bin");
        let home = dev_daemon::spawn_envs(&daemon_spec(&spec, &bin)).expect("the home env builds");
        let remote_spec = remote_daemon_spec(&spec, &bin);
        assert_eq!(remote_spec.name, "unit-remote");
        assert_eq!(remote_spec.scratch_root, spec.scratch.join("remote"));
        assert_eq!(remote_spec.bin_dir, bin);
        assert!(remote_spec.extra_env.is_empty() && remote_spec.daemon_only_env.is_empty());
        let remote = dev_daemon::spawn_envs(&remote_spec).expect("the remote env builds");
        for rows in [&home.shared, &home.daemon, &remote.shared, &remote.daemon] {
            assert!(!names_ssh(rows), "a daemon's environment names an SSH row");
        }
        assert!(
            !remote
                .daemon
                .iter()
                .any(|(name, value)| *name == "RUSTLING_TULIP_PWA_DIR"
                    || value.to_str() == Some(SENTINEL)),
            "the remote daemon got the home spec's rows"
        );

        let ssh = ClientSsh {
            shim: spec
                .repo_root
                .join("tools")
                .join("e2e")
                .join("fake-ssh")
                .join("fake-ssh.cmd"),
            state: spec.scratch.join("fake-ssh").join("state.json"),
        };
        let client = |ssh: Option<&ClientSsh>| {
            let spec = ClientSpec {
                envs: &home.shared,
                bin: &bin,
                scratch: &spec.scratch,
                removed: &[],
                ssh,
            };
            let command = client_command(&spec, Stdio::null());
            command
                .get_envs()
                .filter(|(name, _)| {
                    *name == OsStr::new(SSH_ENV) || *name == OsStr::new(FAKE_SSH_STATE_ENV)
                })
                .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
                .collect::<Vec<_>>()
        };
        let mut set = client(Some(&ssh));
        set.sort();
        assert_eq!(
            set,
            vec![
                (
                    OsString::from(FAKE_SSH_STATE_ENV),
                    Some(ssh.state.clone().into_os_string())
                ),
                (
                    OsString::from(SSH_ENV),
                    Some(ssh.shim.clone().into_os_string())
                ),
            ]
        );
        let mut removed = client(None);
        removed.sort();
        assert_eq!(
            removed,
            vec![
                (OsString::from(FAKE_SSH_STATE_ENV), None),
                (OsString::from(SSH_ENV), None)
            ],
            "without a remote host the client's command removes both"
        );
        assert!(
            !spec.scratch.exists(),
            "building the environments made {}",
            spec.scratch.display()
        );
    }

    /// The fake ssh's state names the remote daemon's exe as the bridge and
    /// its environment, logs under the session's folder, and refuses nothing.
    #[test]
    fn the_fake_ssh_state_names_the_remote_daemon() {
        let spec = unit_spec("state", true);
        let bin = spec.scratch.join("bin");
        let remote = dev_daemon::spawn_envs(&remote_daemon_spec(&spec, &bin))
            .expect("the remote env builds");
        let dir = fixture("fake-ssh-state");
        let bridge = bin.join("rustling-tulipd.exe");

        let written = write_fake_ssh(&dir.join("fake-ssh"), &bridge, &remote.shared);
        let text = std::fs::read_to_string(dir.join("fake-ssh").join("state.json"));
        let _ = std::fs::remove_dir_all(&dir);

        let state_path = written.expect("the state is written");
        assert_eq!(state_path, dir.join("fake-ssh").join("state.json"));
        let state: serde_json::Value =
            serde_json::from_str(&text.expect("the state file is there"))
                .expect("the state is JSON");
        let env: serde_json::Map<String, serde_json::Value> = remote
            .shared
            .iter()
            .map(|(name, value)| ((*name).to_owned(), json!(value.to_string_lossy())))
            .collect();
        assert_eq!(
            state,
            json!({
                "bridge": bridge,
                "env": env,
                "fail": null,
                "log": dir.join("fake-ssh").join("argv.jsonl"),
            })
        );
        let config = remote
            .shared
            .iter()
            .find(|(name, _)| *name == "RUSTLING_TULIP_CONFIG_DIR")
            .map(|(_, value)| PathBuf::from(value));
        assert_eq!(config, Some(spec.scratch.join("remote").join("config")));
    }

    #[test]
    fn the_saved_host_seed_names_only_devbox_and_never_overwrites() {
        let fresh = fixture("ui-fresh");
        let existing = fixture("ui-existing");
        let mine = r#"{"sidebar_collapsed":true}"#;
        std::fs::write(existing.join(UI_FILE), mine).expect("writes the existing file");

        let seeded = seed_saved_host(&fresh);
        let kept = seed_saved_host(&existing);
        let written = std::fs::read(fresh.join(UI_FILE));
        let after = std::fs::read_to_string(existing.join(UI_FILE));
        let _ = std::fs::remove_dir_all(&fresh);
        let _ = std::fs::remove_dir_all(&existing);

        seeded.expect("a fresh folder is seeded");
        kept.expect("an existing file is left alone, not an error");
        let written: serde_json::Value =
            serde_json::from_slice(&written.expect("the seed wrote its file"))
                .expect("the seed is JSON");
        assert_eq!(
            written,
            json!({"saved_hosts": [{"kind": "ssh", "name": "devbox"}]})
        );
        assert_eq!(after.expect("the existing file is still there"), mine);
    }

    /// The workspace root, two levels above this package.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("tools/rt-driver-mcp sits two levels under the repo root")
            .to_path_buf()
    }

    /// The token goes into the daemon's spawn environment only: neither
    /// `IsolatedDaemon::envs`, which the client copies, nor the client's
    /// command carries it.
    #[test]
    fn the_client_never_gets_the_driver_token() {
        const SENTINEL: &str = "sentinel-driver-token-0d5e";
        let root = repo_root();
        let scratch = root
            .join(".tmp")
            .join("driver")
            .join(format!("unit-token-{}", std::process::id()));
        let spec = LaunchSpec {
            session: "unit".to_owned(),
            repo_root: root.clone(),
            build_dir: root.join("no-such-build"),
            scratch: scratch.clone(),
            shell: TestShell::Cmd,
            env: Vec::new(),
            daemon_only_env: vec![(TOKEN_ENV, OsString::from(SENTINEL))],
            remote_host: false,
        };
        let bin = scratch.join("bin");
        let envs = dev_daemon::spawn_envs(&daemon_spec(&spec, &bin))
            .expect("the spec's environment builds");
        let carries = |rows: &[(&'static str, OsString)]| {
            rows.iter()
                .any(|(_, value)| value.to_str() == Some(SENTINEL))
        };
        assert!(
            carries(&envs.daemon),
            "the daemon is spawned with the token"
        );
        assert!(
            !carries(&envs.shared),
            "the environment IsolatedDaemon::envs returns carries the token"
        );

        let client = ClientSpec {
            envs: &envs.shared,
            bin: &bin,
            scratch: &scratch,
            removed: &[],
            ssh: None,
        };
        let command = client_command(&client, Stdio::null());
        let leaked = command.get_envs().any(|(name, value)| {
            name == OsStr::new(TOKEN_ENV) || value == Some(OsStr::new(SENTINEL))
        });
        assert!(!leaked, "the client's command carries the token");
        assert!(
            !scratch.exists(),
            "building the environments made {}",
            scratch.display()
        );
    }

    #[test]
    fn the_seed_writes_only_its_keys_and_never_overwrites() {
        let fresh = fixture("seed-fresh");
        let existing = fixture("seed-existing");
        let mine = r#"{"theme":"light","numStartups":3}"#;
        std::fs::write(existing.join(SEED_FILE), mine).expect("writes the existing file");

        let seeded = seed_onboarding(&fresh);
        let kept = seed_onboarding(&existing);
        let written = std::fs::read(fresh.join(SEED_FILE));
        let after = std::fs::read_to_string(existing.join(SEED_FILE));
        let _ = std::fs::remove_dir_all(&fresh);
        let _ = std::fs::remove_dir_all(&existing);

        seeded.expect("a fresh home is seeded");
        kept.expect("an existing file is left alone, not an error");
        let written: serde_json::Value =
            serde_json::from_slice(&written.expect("the seed wrote its file"))
                .expect("the seed is JSON");
        assert_eq!(
            written,
            json!({"hasCompletedOnboarding": true, "theme": "dark"})
        );
        assert_eq!(after.expect("the existing file is still there"), mine);
    }

    /// A folder of the test's own under the temp dir, made empty.
    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-driver-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("makes the fixture folder");
        dir
    }

    #[test]
    fn discovery_file_with_stale_started_at_is_ignored() {
        let dir = fixture("discovery");
        let file = serde_json::json!({
            "pid": 4242, "pipe": "rt-driver-4242", "started_at": 100, "version": "0.7.0-alpha",
        });
        std::fs::write(dir.join("4242.json"), file.to_string()).expect("writes the discovery file");

        let stale = find_discovery(&dir, 4242, 99);
        let live = find_discovery(&dir, 4242, 100);
        let other = find_discovery(&dir, 4243, 100);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(stale, None, "a start time that differs is another process");
        assert_eq!(other, None, "no file for that pid");
        assert_eq!(
            live,
            Some(Discovery {
                pid: 4242,
                pipe: "rt-driver-4242".to_owned(),
                started_at: 100,
                version: "0.7.0-alpha".to_owned(),
            })
        );
    }

    #[test]
    fn launch_refuses_a_missing_driver_build_naming_the_build_command() {
        let root = fixture("no-build");
        let scratch = root.join(".tmp").join("driver").join("unit-1");
        let spec = LaunchSpec {
            session: "unit".to_owned(),
            repo_root: root.clone(),
            build_dir: root.join("empty-build"),
            scratch: scratch.clone(),
            shell: TestShell::Cmd,
            env: Vec::new(),
            daemon_only_env: Vec::new(),
            remote_host: false,
        };
        std::fs::create_dir_all(&spec.build_dir).expect("makes the empty build folder");

        let refused = launch(&spec).err();
        let scratch_made = scratch.exists();
        let _ = std::fs::remove_dir_all(&root);

        let message = refused.expect("an empty build folder is refused");
        assert!(message.contains(BUILD_HINT), "{message}");
        assert!(
            message.contains("rustling-tulip-native")
                && message.contains("rustling-tulipd")
                && message.contains("rt-tracer"),
            "names each missing exe: {message}"
        );
        assert!(!scratch_made, "a refused launch made {}", scratch.display());
    }

    /// The discovery wait's loop on a manual clock: no client, no sleeping.
    mod discovery_wait {
        use std::sync::Arc;
        use std::time::Duration;

        use load_clock::{DEFAULT_STALL_LIMIT, Deadline, StallWatch};

        use super::super::{DISCOVERY_GUARD, Discovery, Observed, discovery_loop};
        use crate::pipe::tests::{Rig, assert_about};

        const PAUSE: Duration = Duration::from_millis(100);

        fn discovery() -> Discovery {
            Discovery {
                pid: 42,
                pipe: "rt-driver-42".to_owned(),
                started_at: 7,
                version: "1".to_owned(),
            }
        }

        /// A client that neither exits nor writes its file, logging a line each pass when `logs`.
        fn quiet(pass: u64, logs: bool) -> Observed {
            Observed {
                exited: None,
                found: None,
                log_len: if logs { pass } else { 10 },
                cpu: Duration::from_secs(1),
            }
        }

        /// Runs the loop against `budget` at a machine free by `share`, each pass observed by `script`.
        fn run(
            rig: &Rig,
            budget: Duration,
            share: f64,
            mut script: impl FnMut(u64) -> Observed,
        ) -> Result<Discovery, String> {
            let deadline = Deadline::start(Arc::clone(&rig.clock), budget);
            let mut stall = StallWatch::new(rig.ticker.clone(), DEFAULT_STALL_LIMIT);
            let mut pass = 0;
            discovery_loop(
                &deadline,
                &mut stall,
                || {
                    pass += 1;
                    script(pass)
                },
                || rig.advance(PAUSE, share),
                "pid 42 in the folder",
            )
        }

        #[test]
        fn a_discovery_wait_ends_at_its_found_file() {
            let rig = Rig::new();
            let got = run(&rig, DISCOVERY_GUARD, 1.0, |pass| Observed {
                found: (pass == 5).then(discovery),
                ..quiet(pass, false)
            });
            assert_eq!(got, Ok(discovery()));
            assert_eq!(rig.wall(), 4 * PAUSE);
        }

        #[test]
        fn a_client_that_exits_fails_the_discovery_wait_with_its_stderr() {
            let rig = Rig::new();
            let text = "the client exited (exit code: 4); its stderr ends:\ndriver run refused";
            let got = run(&rig, DISCOVERY_GUARD, 1.0, |pass| Observed {
                exited: (pass == 3).then(|| text.to_owned()),
                // A file read on the exit's own pass does not win over it.
                found: (pass == 3).then(discovery),
                ..quiet(pass, false)
            });
            assert_eq!(got, Err(text.to_owned()));
        }

        #[test]
        fn a_stalled_client_fails_the_discovery_wait() {
            let rig = Rig::new();
            let got = run(&rig, Duration::from_secs(600), 1.0, |pass| {
                quiet(pass, false)
            });
            let err = got.err().unwrap_or_default();
            assert!(
                err.starts_with("the client stalled: no log line and no CPU for 120 s"),
                "{err}"
            );
            assert_about(rig.wall(), DEFAULT_STALL_LIMIT);
        }

        #[test]
        fn a_client_that_logs_is_not_stalled() {
            let rig = Rig::new();
            let got = run(&rig, Duration::from_secs(300), 1.0, |pass| {
                quiet(pass, true)
            });
            let err = got.err().unwrap_or_default();
            assert!(
                err.starts_with(
                    "hung: no discovery file for pid 42 in the folder: did not finish within 300 s"
                ),
                "{err}"
            );
            assert_about(rig.wall(), Duration::from_secs(300));
        }

        #[test]
        fn a_loaded_discovery_wait_outlasts_60_s_of_wall() {
            let rig = Rig::new();
            let got = run(&rig, DISCOVERY_GUARD, 0.5, |pass| quiet(pass, true));
            let err = got.err().unwrap_or_default();
            assert!(
                err.starts_with(
                    "hung: no discovery file for pid 42 in the folder: did not finish within 60 s"
                ),
                "{err}"
            );
            assert_about(rig.wall(), Duration::from_secs(120));
        }
    }
}
