//! `launch`: shadow-copy a driver build, start an isolated daemon from the
//! copy, start the driven client against it, and wait until its endpoint
//! answers.

use std::ffi::OsString;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dev_daemon::{DaemonSpec, IsolatedDaemon, TestShell};
use serde::Deserialize;
use serde_json::json;

use crate::pipe::{PipeClient, Transport as _};
use crate::session::{ScratchRepo, Session, log_tail, native_log};
use crate::token;

/// What a missing driver build is told to run.
pub const BUILD_HINT: &str = "build the driven client first: cargo build -p rustling-tulip-native --features driver \
     --profile test-release, then cargo build -p daemon -p tracer --profile test-release (through tools/gate.ps1)";

/// The exes a session runs, shadow-copied out of the build folder.
const EXES: [&str; 3] = ["rustling-tulip-native", "rustling-tulipd", "rt-tracer"];

/// The hang guard of the wait for the client's discovery file. A healthy
/// start writes it after its first cloaked frame, in seconds.
const DISCOVERY_GUARD: Duration = Duration::from_secs(60);

/// The folder under the temp dir with one discovery file per driven client.
const DISCOVERY_FOLDER: &str = "rustling-tulip-driver";

/// Set to `1`, the client opens its driver endpoint.
const DRIVER_ENV: &str = "RUSTLING_TULIP_DRIVER";

/// Set, the client's window opens cloaked and never activates.
const OFFSCREEN_ENV: &str = "RUSTLING_TULIP_OFFSCREEN_WINDOW";

/// The file a real Claude's home is seeded with, so its first start skips
/// onboarding.
pub const SEED_FILE: &str = ".claude.json";

/// What to launch: `scratch` is the session's own folder under the repo's
/// `.tmp/driver/`; `env` is set on the isolated daemon after its isolation
/// variables, so a row can point an agent or the tailnet at a real CLI;
/// `daemon_only_env` is set on the daemon alone, never on the client.
pub struct LaunchSpec {
    pub session: String,
    pub repo_root: PathBuf,
    pub build_dir: PathBuf,
    pub scratch: PathBuf,
    pub shell: TestShell,
    pub env: Vec<(&'static str, OsString)>,
    pub daemon_only_env: Vec<(&'static str, OsString)>,
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
    let log = native_log(&daemon.config_dir());
    let fail = |step: &str, err: &str| {
        format!(
            "{step}: {err}\n\n--- {} ends ---\n{}",
            log.display(),
            log_tail(&log)
        )
    };
    let mut client = spawn_client(&daemon, &bin, &spec.scratch, removed)
        .map_err(|err| fail("starting the client", &err))?;
    let pipe = match wait_discovery(&mut client, &spec.scratch) {
        Ok(discovery) => format!(r"\\.\pipe\{}", discovery.pipe),
        Err(err) => {
            let message = fail("waiting for the driver endpoint", &err);
            drop(Session::new(
                spec.session.clone(),
                client,
                PipeClient::new(String::new()),
                daemon,
                spec.repo_root.clone(),
                scratch_repo,
            ));
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
    );
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
    let path = home.join(SEED_FILE);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path);
    let mut file = match file {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(err) => return Err(format!("seeding {}: {err}", path.display())),
    };
    let seed = json!({"hasCompletedOnboarding": true, "theme": "dark"});
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
fn spawn_client(
    daemon: &IsolatedDaemon,
    bin: &Path,
    scratch: &Path,
    removed: &[&'static str],
) -> Result<Child, String> {
    let stderr_path = scratch.join("client-stderr.log");
    let stderr = File::create(&stderr_path)
        .map_err(|err| format!("creating {}: {err}", stderr_path.display()))?;
    client_command(daemon.envs(), bin, scratch, removed, Stdio::from(stderr))
        .spawn()
        .map_err(|err| format!("spawning the client: {err}"))
}

/// The copied client's command: `envs` (the daemon's [`IsolatedDaemon::envs`],
/// never its daemon-only rows) and the driver switched on, run in `scratch`.
fn client_command(
    envs: &[(&'static str, OsString)],
    bin: &Path,
    scratch: &Path,
    removed: &[&'static str],
    stderr: Stdio,
) -> Command {
    let mut command = Command::new(bin.join(exe("rustling-tulip-native")));
    // The client hands its environment on to any daemon it starts itself.
    for name in removed {
        command.env_remove(*name);
    }
    command
        .envs(envs.iter().map(|(key, value)| (*key, value)))
        .env(DRIVER_ENV, "1")
        .env(OFFSCREEN_ENV, "1")
        .current_dir(scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    dev_daemon::no_console(&mut command);
    command
}

/// Waits for the discovery file of `client`, matched by pid and creation
/// time, failing at once when the client exits.
fn wait_discovery(client: &mut Child, scratch: &Path) -> Result<Discovery, String> {
    let started_at = creation_time(client)?;
    let dir = std::env::temp_dir().join(DISCOVERY_FOLDER);
    let deadline = Instant::now() + DISCOVERY_GUARD;
    loop {
        if let Ok(Some(status)) = client.try_wait() {
            let stderr = log_tail(&scratch.join("client-stderr.log"));
            return Err(format!(
                "the client exited ({status}); its stderr ends:\n{stderr}"
            ));
        }
        if let Some(discovery) = find_discovery(&dir, client.id(), started_at) {
            return Ok(discovery);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "hung: no discovery file for pid {} in {} within {DISCOVERY_GUARD:?}",
                client.id(),
                dir.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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

/// `child`'s creation time as a `FILETIME` in one u64, read through its
/// handle.
#[cfg(windows)]
fn creation_time(child: &Child) -> Result<u64, String> {
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
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

#[cfg(not(windows))]
fn creation_time(_child: &Child) -> Result<u64, String> {
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
        BUILD_HINT, Discovery, LaunchSpec, SEED_FILE, TestShell, client_command, daemon_spec,
        find_discovery, launch, seed_onboarding,
    };
    use crate::token::TOKEN_ENV;

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

        let command = client_command(&envs.shared, &bin, &scratch, &[], Stdio::null());
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
}
