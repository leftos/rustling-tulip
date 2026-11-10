//! The host update's sandbox for the live e2e tier: a daemon built with
//! `e2e-loopback-peers` and started with `RUSTLING_TULIP_E2E_HOST_UPDATE=1`
//! finds its install in `RUSTLING_TULIP_BIN_TEMPLATES`, records the
//! installer's launch to a file instead of starting it, and takes an update
//! lock of its own, so a live spec drives a remote update past its checks
//! without a real installer ever running. The switch swaps all of these
//! together, and refuses to start unless every folder the update writes or
//! reads is isolated under a `.tmp` folder and the feed is a loopback one.

use super::{DAEMON_EXE, InstallLocator, Launch, UpdateLock};
use daemon_client::isolation::{self, NotIsolated, Reason};
use std::ffi::{OsStr, OsString};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The switch: unset or empty is a normal daemon, `1` the sandbox.
const SWITCH: &str = "RUSTLING_TULIP_E2E_HOST_UPDATE";

/// The countdown override, read only with the switch on.
const COUNTDOWN_VAR: &str = "RUSTLING_TULIP_E2E_HOST_UPDATE_COUNTDOWN_SECS";

/// The folder the daemon runs from, which the sandbox's install is.
const BIN_TEMPLATES_VAR: &str = "RUSTLING_TULIP_BIN_TEMPLATES";

/// The release feed's base, which must be a loopback origin.
const FEED_VAR: &str = "RUSTLING_TULIP_UPDATE_FEED";

/// The updates folder downloads go under.
const UPDATES_VAR: &str = "RUSTLING_TULIP_UPDATES_DIR";

/// The config folder, which holds the launch record.
const CONFIG_VAR: &str = "RUSTLING_TULIP_CONFIG_DIR";

/// The launch record's file name in the config folder.
const RECORD_FILE: &str = "e2e-host-update-launch.json";

/// The longest countdown the override accepts, in seconds.
const MAX_COUNTDOWN_SECS: u64 = 600;

/// What a sandboxed host update runs through: the install folder, the file
/// the launch is recorded to, the update lock's name, and the countdown when
/// it is overridden.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the switch"
    )
)]
pub(super) struct SandboxPlan {
    pub(super) install: PathBuf,
    pub(super) record: PathBuf,
    pub(super) lock_name: String,
    pub(super) countdown: Option<Duration>,
}

/// The sandbox the switch asks for, read through `read`: `None` when it is
/// unset or empty, the plan when it is `1` and the daemon is isolated.
///
/// # Errors
///
/// A switch of any other value, or `1` without an isolated config, binaries,
/// worktrees or updates folder, an install folder under `.tmp` holding the
/// daemon, a loopback feed, or with a countdown override outside `1..=600`.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the switch"
    )
)]
pub(super) fn request(
    read: impl Fn(&str) -> Option<OsString>,
) -> anyhow::Result<Option<SandboxPlan>> {
    let switch = read(SWITCH).unwrap_or_default();
    if switch.is_empty() {
        return Ok(None);
    }
    if switch != "1" {
        anyhow::bail!(
            "{SWITCH} must be unset, empty or 1, got \"{}\"; refusing to start",
            switch.to_string_lossy()
        );
    }
    isolation::check_isolated(&read).map_err(not_isolated)?;
    isolation::check_isolated_var(UPDATES_VAR, &read).map_err(not_isolated)?;
    // Their later readers use `std::env::var`, which takes a value that is
    // not Unicode for unset and falls back to the machine's real folder.
    for variable in [CONFIG_VAR, UPDATES_VAR] {
        if read(variable).is_some_and(|value| value.to_str().is_none()) {
            return Err(refusal(variable, "to be valid Unicode"));
        }
    }
    let install = install_folder(read(BIN_TEMPLATES_VAR))?;
    check_feed(read(FEED_VAR))?;
    let countdown = countdown(read(COUNTDOWN_VAR))?;
    // `check_isolated` has passed, so the config folder is set.
    let config = PathBuf::from(read(CONFIG_VAR).unwrap_or_default());
    Ok(Some(SandboxPlan {
        install,
        record: config.join(RECORD_FILE),
        lock_name: lock_name(config.as_os_str()),
        countdown,
    }))
}

/// The refusal for a folder variable that is not isolated.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the switch"
    )
)]
fn not_isolated(err: NotIsolated) -> anyhow::Error {
    let why = match err.reason {
        Reason::Unset => "it is not set".to_owned(),
        Reason::Empty => "it is empty".to_owned(),
        Reason::Relative(path) => format!("it is a relative path ({})", path.display()),
        Reason::NotUnderTmp(path) => format!("{} is not under one", path.display()),
    };
    refusal(
        err.variable,
        &format!("set to an absolute folder under a .tmp folder, but {why}"),
    )
}

/// A refusal naming `variable` and why.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the switch"
    )
)]
fn refusal(variable: &str, reason: &str) -> anyhow::Error {
    anyhow::anyhow!("{SWITCH}=1 needs {variable} {reason}; refusing to start")
}

/// The install folder `RUSTLING_TULIP_BIN_TEMPLATES` names, when it is
/// absolute, under `.tmp` and holds the daemon.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the switch"
    )
)]
fn install_folder(value: Option<OsString>) -> anyhow::Result<PathBuf> {
    let folder = PathBuf::from(value.unwrap_or_default());
    if !folder.is_absolute() || !isolation::under_tmp(&folder) {
        return Err(refusal(
            BIN_TEMPLATES_VAR,
            &format!(
                "set to an absolute folder under a .tmp folder, but it is \"{}\"",
                folder.display()
            ),
        ));
    }
    if !folder.join(DAEMON_EXE).is_file() {
        return Err(refusal(
            BIN_TEMPLATES_VAR,
            &format!("to hold {DAEMON_EXE}, but {} does not", folder.display()),
        ));
    }
    Ok(folder)
}

/// Refused unless the feed is set to a loopback http(s) origin.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the switch"
    )
)]
fn check_feed(value: Option<OsString>) -> anyhow::Result<()> {
    let value = value.unwrap_or_default();
    let loopback = value
        .to_str()
        .is_some_and(super::super::accounts::is_loopback_origin);
    if loopback {
        return Ok(());
    }
    Err(refusal(
        FEED_VAR,
        &format!(
            "set to a loopback http(s) origin, but it is \"{}\"",
            value.to_string_lossy()
        ),
    ))
}

/// The countdown override: `None` when unset, else `1..=600` seconds.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the switch"
    )
)]
fn countdown(value: Option<OsString>) -> anyhow::Result<Option<Duration>> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value.to_str().and_then(|text| text.parse::<u64>().ok()) {
        Some(secs @ 1..=MAX_COUNTDOWN_SECS) => Ok(Some(Duration::from_secs(secs))),
        _ => Err(refusal(
            COUNTDOWN_VAR,
            &format!(
                "unset or a whole number of seconds from 1 to {MAX_COUNTDOWN_SECS}, but it is \"{}\"",
                value.to_string_lossy()
            ),
        )),
    }
}

/// The sandbox's update lock name for the daemon whose config folder is
/// `config`: its own, never the machine's `Local\rustling-tulip-update`.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the switch"
    )
)]
fn lock_name(config: &OsStr) -> String {
    let mut hasher = DefaultHasher::new();
    config.hash(&mut hasher);
    format!(r"Local\rustling-tulip-update-e2e-{:016x}", hasher.finish())
}

/// The sandbox's install: the folder the daemon runs from, when it holds
/// the daemon.
#[cfg_attr(
    not(feature = "e2e-loopback-peers"),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build sandboxes the update"
    )
)]
pub(super) struct SandboxInstall {
    pub(super) folder: PathBuf,
}

impl InstallLocator for SandboxInstall {
    fn install_folder(&self) -> Option<PathBuf> {
        self.folder
            .join(DAEMON_EXE)
            .is_file()
            .then(|| self.folder.clone())
    }
}

/// A launch that records the program and its arguments to `file` as JSON
/// and starts nothing.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the switch"
    )
)]
pub(super) struct RecordLaunch {
    pub(super) file: PathBuf,
}

impl Launch for RecordLaunch {
    fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()> {
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let record = serde_json::json!({
            "program": program.display().to_string(),
            "args": args,
        });
        let bytes = serde_json::to_vec_pretty(&record)?;
        crate::atomic_file::write_atomic(&self.file, &bytes)
    }
}

/// An update lock of the sandbox's own name.
#[cfg_attr(
    not(feature = "e2e-loopback-peers"),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build sandboxes the update"
    )
)]
pub(super) struct NamedLock {
    pub(super) name: String,
}

impl UpdateLock for NamedLock {
    fn try_lock(&self) -> anyhow::Result<Option<Box<dyn std::any::Any>>> {
        update_core::try_lock(&self.name)
    }
}

#[cfg(test)]
#[expect(
    clippy::panic,
    reason = "tests fail with the result they got; panic messages aid debugging"
)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// A scratch root under `<temp>/.tmp/<uuid>`, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(".tmp")
                .join(uuid::Uuid::new_v4().to_string());
            std::fs::create_dir_all(&root).unwrap_or_else(|err| panic!("scratch root: {err}"));
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The scratch root and every variable a sandboxed daemon needs, set to
    /// isolated folders that exist, the install holding the daemon.
    fn isolated() -> (Scratch, HashMap<&'static str, OsString>) {
        let scratch = Scratch::new();
        let root = scratch.path();
        for folder in ["config", "binaries", "worktrees", "updates", "install"] {
            std::fs::create_dir_all(root.join(folder))
                .unwrap_or_else(|err| panic!("{folder}: {err}"));
        }
        std::fs::write(root.join("install").join(DAEMON_EXE), b"")
            .unwrap_or_else(|err| panic!("daemon exe: {err}"));
        let vars = HashMap::from([
            ("RUSTLING_TULIP_E2E_HOST_UPDATE", OsString::from("1")),
            ("RUSTLING_TULIP_CONFIG_DIR", root.join("config").into()),
            ("RUSTLING_TULIP_BINARIES_DIR", root.join("binaries").into()),
            (
                "RUSTLING_TULIP_WORKTREES_DIR",
                root.join("worktrees").into(),
            ),
            ("RUSTLING_TULIP_UPDATES_DIR", root.join("updates").into()),
            ("RUSTLING_TULIP_BIN_TEMPLATES", root.join("install").into()),
            (
                "RUSTLING_TULIP_UPDATE_FEED",
                OsString::from("http://127.0.0.1:9"),
            ),
        ]);
        (scratch, vars)
    }

    fn ask(vars: &HashMap<&'static str, OsString>) -> anyhow::Result<Option<SandboxPlan>> {
        request(|name| vars.get(name).cloned())
    }

    /// The refusal `vars` gets, as text.
    fn refused(vars: &HashMap<&'static str, OsString>) -> String {
        match ask(vars) {
            Ok(plan) => panic!("expected a refusal, got {:?}", plan.is_some()),
            Err(err) => format!("{err:#}"),
        }
    }

    #[test]
    fn an_unset_switch_is_no_sandbox() {
        let vars = HashMap::new();
        assert!(matches!(ask(&vars), Ok(None)));
    }

    #[test]
    fn an_empty_switch_is_no_sandbox() {
        let vars = HashMap::from([("RUSTLING_TULIP_E2E_HOST_UPDATE", OsString::new())]);
        assert!(matches!(ask(&vars), Ok(None)));
    }

    #[test]
    fn a_switch_of_another_value_refuses_to_start() {
        let (_scratch, mut vars) = isolated();
        vars.insert("RUSTLING_TULIP_E2E_HOST_UPDATE", OsString::from("yes"));
        let err = refused(&vars);
        assert!(err.contains("must be unset, empty or 1"), "{err}");
        assert!(err.contains("\"yes\""), "{err}");
    }

    #[test]
    fn the_switch_refuses_a_config_dir_outside_tmp() {
        let (_scratch, mut vars) = isolated();
        let outside = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        vars.insert("RUSTLING_TULIP_CONFIG_DIR", outside.into());
        let err = refused(&vars);
        assert!(err.contains("RUSTLING_TULIP_CONFIG_DIR"), "{err}");
    }

    #[test]
    fn the_switch_refuses_an_updates_dir_that_is_not_isolated() {
        let (_scratch, mut vars) = isolated();
        vars.remove("RUSTLING_TULIP_UPDATES_DIR");
        let err = refused(&vars);
        assert!(err.contains("RUSTLING_TULIP_UPDATES_DIR"), "{err}");
    }

    #[test]
    fn the_switch_refuses_bin_templates_without_the_daemon() {
        let (scratch, vars) = isolated();
        std::fs::remove_file(scratch.path().join("install").join(DAEMON_EXE))
            .unwrap_or_else(|err| panic!("remove the daemon exe: {err}"));
        let err = refused(&vars);
        assert!(err.contains("RUSTLING_TULIP_BIN_TEMPLATES"), "{err}");
    }

    #[test]
    fn the_switch_refuses_bin_templates_outside_tmp() {
        let (_scratch, mut vars) = isolated();
        let outside = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        vars.insert("RUSTLING_TULIP_BIN_TEMPLATES", outside.into());
        let err = refused(&vars);
        assert!(err.contains("RUSTLING_TULIP_BIN_TEMPLATES"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn a_non_unicode_updates_dir_refuses_to_start() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let (scratch, mut vars) = isolated();
        let mut wide: Vec<u16> = scratch
            .path()
            .join("updates")
            .as_os_str()
            .encode_wide()
            .collect();
        wide.extend([u16::from(b'\\'), 0xD800, u16::from(b'x')]);
        vars.insert("RUSTLING_TULIP_UPDATES_DIR", OsString::from_wide(&wide));
        let err = refused(&vars);
        assert!(err.contains("RUSTLING_TULIP_UPDATES_DIR"), "{err}");
        assert!(err.contains("valid Unicode"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn a_non_unicode_config_dir_refuses_to_start() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let (scratch, mut vars) = isolated();
        let mut wide: Vec<u16> = scratch
            .path()
            .join("config")
            .as_os_str()
            .encode_wide()
            .collect();
        wide.extend([u16::from(b'\\'), 0xD800, u16::from(b'x')]);
        vars.insert("RUSTLING_TULIP_CONFIG_DIR", OsString::from_wide(&wide));
        let err = refused(&vars);
        assert!(err.contains("RUSTLING_TULIP_CONFIG_DIR"), "{err}");
        assert!(err.contains("valid Unicode"), "{err}");
    }

    #[test]
    fn the_switch_refuses_a_feed_that_is_not_loopback() {
        let (_scratch, mut vars) = isolated();
        vars.insert(
            "RUSTLING_TULIP_UPDATE_FEED",
            OsString::from("https://api.github.com"),
        );
        let err = refused(&vars);
        assert!(err.contains("RUSTLING_TULIP_UPDATE_FEED"), "{err}");
    }

    #[test]
    fn the_switch_refuses_a_missing_feed() {
        let (_scratch, mut vars) = isolated();
        vars.remove("RUSTLING_TULIP_UPDATE_FEED");
        let err = refused(&vars);
        assert!(err.contains("RUSTLING_TULIP_UPDATE_FEED"), "{err}");
    }

    #[test]
    fn an_isolated_switch_plans_the_sandbox() {
        let (scratch, vars) = isolated();
        let plan = ask(&vars)
            .unwrap_or_else(|err| panic!("expected a plan, got {err:#}"))
            .unwrap_or_else(|| panic!("expected a plan, got none"));
        assert_eq!(plan.install, scratch.path().join("install"));
        assert_eq!(
            plan.record,
            scratch
                .path()
                .join("config")
                .join("e2e-host-update-launch.json")
        );
        assert!(
            plan.lock_name
                .starts_with(r"Local\rustling-tulip-update-e2e-"),
            "{}",
            plan.lock_name
        );
        assert_ne!(plan.lock_name, update_core::UPDATE_MUTEX);
        assert_eq!(plan.countdown, None);
    }

    #[test]
    fn the_countdown_override_reads_seconds() {
        let (_scratch, mut vars) = isolated();
        vars.insert(
            "RUSTLING_TULIP_E2E_HOST_UPDATE_COUNTDOWN_SECS",
            OsString::from("5"),
        );
        let plan = ask(&vars)
            .unwrap_or_else(|err| panic!("expected a plan, got {err:#}"))
            .unwrap_or_else(|| panic!("expected a plan, got none"));
        assert_eq!(plan.countdown, Some(Duration::from_secs(5)));
        for bad in ["0", "601", "x"] {
            vars.insert(
                "RUSTLING_TULIP_E2E_HOST_UPDATE_COUNTDOWN_SECS",
                OsString::from(bad),
            );
            let err = refused(&vars);
            assert!(
                err.contains("RUSTLING_TULIP_E2E_HOST_UPDATE_COUNTDOWN_SECS"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn the_recorder_writes_the_program_and_its_arguments_and_starts_nothing() {
        let scratch = Scratch::new();
        let file = scratch.path().join("e2e-host-update-launch.json");
        let recorder = RecordLaunch { file: file.clone() };
        let args: Vec<OsString> = ["/S", "/UPDATE"].iter().map(OsString::from).collect();

        recorder
            .launch(Path::new(r"C:\nowhere\setup.exe"), &args)
            .unwrap_or_else(|err| panic!("record: {err:#}"));

        let bytes = std::fs::read(&file).unwrap_or_else(|err| panic!("read the record: {err}"));
        let record: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or_else(|err| panic!("parse the record: {err}"));
        assert_eq!(
            record,
            serde_json::json!({"program": r"C:\nowhere\setup.exe", "args": ["/S", "/UPDATE"]})
        );
    }

    #[test]
    fn the_recorder_reports_a_failed_write() {
        let scratch = Scratch::new();
        let blocker = scratch.path().join("a-file");
        std::fs::write(&blocker, b"").unwrap_or_else(|err| panic!("blocker: {err}"));
        let recorder = RecordLaunch {
            file: blocker.join("e2e-host-update-launch.json"),
        };

        let result = recorder.launch(Path::new(r"C:\nowhere\setup.exe"), &[]);

        assert!(result.is_err(), "a write under a file must fail");
    }
}
