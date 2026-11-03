//! The environment that isolates a daemon to its scratch folder, and the
//! folders, agent homes and `PATH` stubs it names.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use agent_api::{AgentPlugin, FakeCli};
use protocol::AgentId;

use crate::{DaemonSpec, LaunchError};

/// What a `PATH` stub prints before its arguments.
pub const STUB_MARKER: &str = "[rt-e2e agent stub]";

/// The daemon's directories under the scratch root.
const SCRATCH_DIRS: [&str; 6] = [
    "config",
    "binaries",
    "worktrees",
    "tailnet",
    "ui",
    "fake-bin",
];

/// The variables that isolate a daemon to `spec`'s scratch root, with
/// `spec.extra_env` after them so one can override another. Nothing is made
/// on disk.
pub(crate) fn isolated_envs(
    spec: &DaemonSpec,
) -> Result<Vec<(&'static str, OsString)>, LaunchError> {
    let root = &spec.scratch_root;
    let fake_tailscale = if cfg!(windows) {
        "fake-tailscale.cmd"
    } else {
        "fake-tailscale.sh"
    };
    let fake_tailscale = spec
        .repo_root
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
            OsString::from(format!(
                "rt-native-e2e-{}-{}",
                std::process::id(),
                spec.name
            )),
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
        ("PATH", path_with_stub(root)?),
        // The tracer the daemon copies into its binaries dir: the one built
        // beside it, never the installed app's.
        (
            "RUSTLING_TULIP_BIN_TEMPLATES",
            spec.bin_dir.clone().into_os_string(),
        ),
    ];
    // Every registered agent runs its fake, with its home under the scratch
    // root: never the user's real CLI or its files.
    for plugin in agent_registry::all() {
        let fake = fake_cli(*plugin)?;
        envs.push((plugin.program_env_var(), fake.shim.into_os_string()));
        if let Some(home_env) = fake.home_env {
            let home = agent_home(root, &plugin.descriptor().id);
            envs.push((home_env, home.into_os_string()));
        }
    }
    envs.extend(spec.extra_env.iter().cloned());
    Ok(envs)
}

/// The value `envs` gives `name`, the last one winning as on a child's
/// `Command::envs`.
pub(crate) fn env_value(envs: &[(&'static str, OsString)], name: &str) -> Option<OsString> {
    envs.iter()
        .rev()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.clone())
}

/// Makes the daemon's directories under `root`, each registered agent's
/// home folder and its `PATH` stub, keeping whatever `root` already holds.
pub(crate) fn make_scratch_dirs(root: &Path) -> Result<(), LaunchError> {
    for sub in SCRATCH_DIRS {
        create_dir(&root.join(sub))?;
    }
    for plugin in agent_registry::all() {
        let fake = fake_cli(*plugin)?;
        if fake.home_env.is_some() {
            create_dir(&agent_home(root, &plugin.descriptor().id))?;
        }
        if let Some(program) = fake.path_stub {
            write_path_stub(&root.join("fake-bin"), program)?;
        }
    }
    Ok(())
}

fn create_dir(path: &Path) -> Result<(), LaunchError> {
    std::fs::create_dir_all(path).map_err(|source| LaunchError::Scratch {
        path: path.to_path_buf(),
        source,
    })
}

/// A shell whose prompt and echo are the same on every run.
fn test_shell() -> OsString {
    std::env::var_os("ComSpec").unwrap_or_else(|| OsString::from("cmd.exe"))
}

/// A `program` in `dir` that prints [`STUB_MARKER`] and its arguments, so a
/// shell the daemon starts never runs the user's real CLI.
#[cfg(windows)]
fn write_path_stub(dir: &Path, program: &str) -> Result<(), LaunchError> {
    let path = dir.join(format!("{program}.cmd"));
    let script = format!("@echo off\r\necho {STUB_MARKER} %*\r\n");
    std::fs::write(&path, script).map_err(|source| LaunchError::Scratch { path, source })
}

#[cfg(not(windows))]
fn write_path_stub(dir: &Path, program: &str) -> Result<(), LaunchError> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(program);
    let script = format!("#!/bin/sh\necho '{STUB_MARKER}' \"$@\"\n");
    std::fs::write(&path, script)
        .and_then(|()| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)))
        .map_err(|source| LaunchError::Scratch { path, source })
}

/// `plugin`'s fake CLI; an isolated daemon runs no agent without one.
fn fake_cli(plugin: &dyn AgentPlugin) -> Result<FakeCli, LaunchError> {
    plugin.fake_cli().ok_or_else(|| LaunchError::NoFakeCli {
        agent: plugin.descriptor().id.0.clone(),
    })
}

/// `<root>/<agent id>-home`: the folder `agent`'s fake `home_env` names.
pub(crate) fn agent_home(root: &Path, agent: &AgentId) -> PathBuf {
    root.join(format!("{}-home", agent.0))
}

/// `PATH` with the stubs' dir first.
fn path_with_stub(root: &Path) -> Result<OsString, LaunchError> {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(root.join("fake-bin")).chain(std::env::split_paths(&inherited));
    Ok(std::env::join_paths(dirs)?)
}
