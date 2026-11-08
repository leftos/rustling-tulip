//! The environment that isolates a daemon to its scratch folder, and the
//! folders, agent homes and `PATH` stubs it names.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use agent_api::{AgentPlugin, FakeCli};
use daemon_client::isolation::check_isolated;
use protocol::AgentId;

use crate::{DaemonSpec, LaunchError, TestShell};

/// What a `PATH` stub prints before its arguments.
pub const STUB_MARKER: &str = "[rt-e2e agent stub]";

/// The daemon's directories under the scratch root.
const SCRATCH_DIRS: [&str; 7] = [
    "config",
    "binaries",
    "worktrees",
    "tailnet",
    "push",
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
        // The tailnet listener's CLI and certificate dir: never the user's
        // Tailscale or their real certificate, even with remote access off.
        ("RUSTLING_TULIP_TAILSCALE", fake_tailscale.into_os_string()),
        (
            "RUSTLING_TULIP_TAILNET_DIR",
            root.join("tailnet").into_os_string(),
        ),
        // The push folder: the live tier never writes the user's real VAPID
        // key or subscriptions.
        (
            "RUSTLING_TULIP_PUSH_DIR",
            root.join("push").into_os_string(),
        ),
        // The account store: never the user's saved logins. The store
        // creates the folder itself, owner-only, when it first opens it.
        (
            "RUSTLING_TULIP_ACCOUNTS_DIR",
            root.join("accounts").into_os_string(),
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
    // A `Cmd` spec's prompt and echo are the same on every run. A `Default`
    // spec names neither, so the daemon uses its Default shell setting, which
    // a fresh config dir auto-detects, and keeps shell integration on.
    if spec.shell == TestShell::Cmd {
        envs.push(("RUSTLING_TULIP_SHELL", test_shell()));
        envs.push(("RUSTLING_TULIP_SHELL_INTEGRATION", OsString::from("0")));
    }
    // Every registered agent runs its fake, with its home under the scratch
    // root: never the user's real CLI or its files. The fake is the one in
    // the checkout the launch names, not the one the harness was built in.
    for plugin in agent_registry::all() {
        let fake = fake_cli(*plugin)?;
        envs.push((
            plugin.program_env_var(),
            spec.repo_root.join(&fake.shim_in_repo).into_os_string(),
        ));
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

/// Refuses `envs` when the final value of any registered agent's home
/// variable is not an absolute path under a `.tmp` folder, by the check the
/// isolation folders get, naming that variable. A row of `extra_env` cannot
/// point an agent at the user's own home.
pub(crate) fn check_agent_homes(envs: &[(&'static str, OsString)]) -> Result<(), LaunchError> {
    for plugin in agent_registry::all() {
        let Some(home_env) = fake_cli(*plugin)?.home_env else {
            continue;
        };
        let value = env_value(envs, home_env);
        check_isolated(|_| value.clone()).map_err(|err| LaunchError::NotIsolated {
            variable: home_env,
            reason: err.reason,
        })?;
    }
    Ok(())
}

/// The scratch folders an elevated daemon checks as protected folders (its
/// config and binaries overrides), so an elevated run creates them protected.
const PROTECTED_SCRATCH_DIRS: [&str; 2] = ["config", "binaries"];

/// Makes the daemon's directories under `root`, each registered agent's
/// home folder and its `PATH` stub, keeping whatever `root` already holds.
/// When this process is elevated, the config and binaries dirs are made (or
/// checked) as protected roots, as the elevated daemon it starts requires of
/// its overrides.
pub(crate) fn make_scratch_dirs(root: &Path) -> Result<(), LaunchError> {
    let elevated = daemon_client::profile::own_profile().map_err(|err| LaunchError::Scratch {
        path: root.to_path_buf(),
        source: std::io::Error::other(format!("reading this process's integrity: {err:#}")),
    })? == daemon_client::profile::Profile::Elevated;
    for sub in SCRATCH_DIRS {
        let path = root.join(sub);
        if elevated && PROTECTED_SCRATCH_DIRS.contains(&sub) {
            create_protected_dir(&path)?;
        } else {
            create_dir(&path)?;
        }
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

/// Creates `path` as a protected root, or checks the one already there
/// ([`daemon_client::protected_root::create_or_check`]).
fn create_protected_dir(path: &Path) -> Result<(), LaunchError> {
    daemon_client::protected_root::create_or_check(path).map_err(|err| LaunchError::Scratch {
        path: path.to_path_buf(),
        source: std::io::Error::other(format!("{err:#}")),
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test asserts its preconditions with expect"
)]
mod tests {
    use std::path::PathBuf;

    use super::{SCRATCH_DIRS, env_value, isolated_envs};
    use crate::{DaemonSpec, TestShell};

    fn spec(scratch_root: PathBuf) -> DaemonSpec {
        DaemonSpec {
            name: "unit".to_owned(),
            scratch_root,
            repo_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            bin_dir: PathBuf::from("no-such-profile"),
            extra_env: Vec::new(),
            daemon_only_env: Vec::new(),
            shell: TestShell::Cmd,
        }
    }

    #[test]
    fn the_push_dir_is_isolated_under_the_scratch_root() {
        let root = std::env::temp_dir().join(format!("rt-push-env-{}", std::process::id()));

        let envs =
            isolated_envs(&spec(root.clone())).expect("every registered agent has a fake CLI");

        assert!(
            SCRATCH_DIRS.contains(&"push"),
            "the scratch root makes a push folder"
        );
        assert_eq!(
            env_value(&envs, "RUSTLING_TULIP_PUSH_DIR"),
            Some(root.join("push").into_os_string()),
            "the live tier never writes the user's real push keys"
        );
    }
}
