//! A macOS update's staging: the downloaded zip is unpacked into a fresh
//! hidden folder beside the installed app bundle, and the staged daemon's
//! `apply-update` swaps the staged app in. The client's own update and the
//! daemon's remote update both stage this way.
//!
//! The path logic and the argument list build on every system, since the
//! daemon picks its platform at run time; the checks that touch the disk
//! build on macOS and in tests; the unpack builds on macOS only.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The prefix of the hidden folder a macOS update unpacks the downloaded app
/// into, beside the installed bundle: the next launch removes it.
pub const STAGING_PREFIX: &str = ".rustling-tulip-update-";

/// The folder the downloaded zip holds at its top level.
pub const BUNDLE_NAME: &str = "rustling-tulip.app";

/// The daemon's file name in a bundle's `Contents/MacOS`.
pub const BUNDLE_DAEMON: &str = "rustling-tulipd";

/// The probe file a start writes into the bundle's folder and removes again.
#[cfg(any(test, target_os = "macos"))]
pub const UPDATE_PROBE: &str = ".rustling-tulip-update-probe";

/// What a start refuses with when the bundle's place cannot be replaced.
#[cfg(any(test, target_os = "macos"))]
pub const UPDATE_REFUSED: &str = "rustling-tulip.app runs from a place the update cannot replace; \
                                  move it to Applications and update again; nothing was changed";

/// The system's zip tool: it keeps the bundle's symlinks and permissions.
#[cfg(target_os = "macos")]
const DITTO: &str = "/usr/bin/ditto";

/// A bound on unpacking the downloaded bundle.
#[cfg(target_os = "macos")]
const DITTO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The app bundle `exe` runs from, for an exe at
/// `<bundle>/Contents/MacOS/<exe>`; `None` for any other layout. Pure path
/// logic: nothing is read from disk and nothing is canonicalized, so a
/// translocated bundle's path stays as the system gave it.
#[must_use]
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()? != std::ffi::OsStr::new("MacOS") {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != std::ffi::OsStr::new("Contents") {
        return None;
    }
    let bundle = contents.parent()?;
    let is_app = Path::new(bundle.file_name()?)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("app"));
    if is_app {
        Some(bundle.to_path_buf())
    } else {
        None
    }
}

/// The daemon inside the staged app at `staged_app`, which runs the swap.
#[must_use]
pub fn staged_daemon(staged_app: &Path) -> PathBuf {
    staged_app
        .join("Contents")
        .join("MacOS")
        .join(BUNDLE_DAEMON)
}

/// The arguments the staged daemon runs: swap `staged_app` for `bundle`,
/// recording `version` as the one replaced; `--restart-daemon` when
/// `restart_daemon`, then `--relaunch` when `relaunch`.
#[must_use]
pub fn apply_update_args(
    staged_app: &Path,
    bundle: &Path,
    version: &str,
    restart_daemon: bool,
    relaunch: bool,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "apply-update".into(),
        "--from".into(),
        staged_app.into(),
        "--to".into(),
        bundle.into(),
        "--old-version".into(),
        version.into(),
        "--update".into(),
    ];
    if restart_daemon {
        args.push("--restart-daemon".into());
    }
    if relaunch {
        args.push("--relaunch".into());
    }
    args
}

/// Refuses a bundle the update cannot replace: one the system runs
/// translocated, or one whose folder this process cannot write a probe file
/// in.
///
/// # Errors
///
/// [`UPDATE_REFUSED`] for a bundle the update cannot replace.
#[cfg(any(test, target_os = "macos"))]
pub fn check_update_place(bundle: &Path) -> anyhow::Result<()> {
    let translocated = bundle
        .components()
        .any(|part| part.as_os_str() == std::ffi::OsStr::new("AppTranslocation"));
    if translocated {
        anyhow::bail!("{UPDATE_REFUSED}");
    }
    let Some(folder) = bundle.parent() else {
        anyhow::bail!("{UPDATE_REFUSED}");
    };
    let probe = folder.join(UPDATE_PROBE);
    let writable = match std::fs::write(&probe, b"") {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(
                path = %probe.display(),
                "update: cannot write beside the app bundle: {err}"
            );
            false
        }
    };
    if let Err(err) = std::fs::remove_file(&probe) {
        tracing::warn!(path = %probe.display(), "update: could not remove the probe file: {err}");
    }
    if writable {
        Ok(())
    } else {
        anyhow::bail!("{UPDATE_REFUSED}")
    }
}

/// A new, empty staging folder beside `bundle`, on the same volume so
/// `apply-update` can rename the staged app into place.
///
/// # Errors
///
/// Fails when `bundle` has no parent folder or no staging folder can be
/// created there.
#[cfg(any(test, target_os = "macos"))]
pub fn fresh_staging_folder(bundle: &Path, version: &semver::Version) -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;
    let folder = bundle
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} is not inside a folder", bundle.display()))?;
    for _ in 0..8 {
        let random: String = uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect();
        let staging = folder.join(format!("{STAGING_PREFIX}{version}-{random}"));
        match std::fs::create_dir(&staging) {
            Ok(()) => return Ok(staging),
            // Another update took this suffix: draw a new one.
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", staging.display()));
            }
        }
    }
    anyhow::bail!("could not create a staging folder in {}", folder.display())
}

/// Stages the downloaded `zip` of `version` beside `bundle`: checks the
/// bundle's place, unpacks the zip into a fresh staging folder with `ditto`,
/// and checks the staged app holds its daemon. The staged app's path; on a
/// failure after the staging folder exists, that folder is removed.
///
/// # Errors
///
/// [`UPDATE_REFUSED`] for a bundle the update cannot replace, or why the
/// staging folder could not be made or the zip not unpacked into it.
#[cfg(target_os = "macos")]
pub fn stage(zip: &Path, bundle: &Path, version: &semver::Version) -> anyhow::Result<PathBuf> {
    check_update_place(bundle)?;
    let staging = fresh_staging_folder(bundle, version)?;
    match unpack(zip, &staging) {
        Ok(staged_app) => Ok(staged_app),
        Err(err) => {
            remove_staging(&staging);
            Err(err)
        }
    }
}

/// Deletes the staging folder `staging`, logging a failure at warn.
#[cfg(target_os = "macos")]
pub fn remove_staging(staging: &Path) {
    if let Err(cleanup) = std::fs::remove_dir_all(staging) {
        tracing::warn!(
            folder = %staging.display(),
            "update: could not delete the staging folder: {cleanup}"
        );
    }
}

/// Unpacks `zip` into `staging`; the staged app, once it holds its daemon.
#[cfg(target_os = "macos")]
fn unpack(zip: &Path, staging: &Path) -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;
    let mut ditto = std::process::Command::new(DITTO);
    ditto.arg("-x").arg("-k").arg(zip).arg(staging);
    run_bounded(&mut ditto, DITTO_TIMEOUT).context("unpacking the update")?;
    let staged_app = staging.join(BUNDLE_NAME);
    if !staged_daemon(&staged_app).is_file() {
        anyhow::bail!("the update archive holds no rustling-tulip.app");
    }
    Ok(staged_app)
}

/// Runs `command` to its own exit, killing it once `timeout` passes.
#[cfg(target_os = "macos")]
fn run_bounded(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    const POLL: std::time::Duration = std::time::Duration::from_millis(20);
    let program = command.get_program().to_string_lossy().into_owned();
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => anyhow::bail!("{program} failed: {status}"),
            Ok(None) => {}
            Err(err) => {
                stop_child(&mut child, &program);
                return Err(err).with_context(|| format!("waiting on {program}"));
            }
        }
        if started.elapsed() >= timeout {
            stop_child(&mut child, &program);
            anyhow::bail!("{program} did not finish within {} s", timeout.as_secs());
        }
        std::thread::sleep(POLL);
    }
}

/// Kills `child`, logging a kill that fails; its exit is then reaped.
#[cfg(target_os = "macos")]
fn stop_child(child: &mut std::process::Child, program: &str) {
    if let Err(err) = child.kill() {
        tracing::warn!("update: could not stop {program}: {err}");
    }
    if let Err(err) = child.wait() {
        tracing::warn!("update: could not reap {program}: {err}");
    }
}

#[cfg(test)]
#[cfg_attr(
    target_os = "macos",
    expect(
        clippy::expect_used,
        reason = "tests assert preconditions with expect; failure messages aid debugging"
    )
)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::{BUNDLE_NAME, STAGING_PREFIX, UPDATE_PROBE, UPDATE_REFUSED, run_bounded, stage};
    use super::{apply_update_args, bundle_of};
    use std::path::{Path, PathBuf};
    #[cfg(target_os = "macos")]
    use std::time::Duration;

    /// Only a hang guard: no test asserts how long anything took.
    #[cfg(target_os = "macos")]
    const HANG_GUARD: Duration = Duration::from_secs(60);

    #[cfg(target_os = "macos")]
    fn v(text: &str) -> semver::Version {
        semver::Version::parse(text).expect("a semver version")
    }

    /// A fresh scratch folder, not yet created.
    #[cfg(target_os = "macos")]
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rt-update-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[cfg(target_os = "macos")]
    fn remove(root: &Path) {
        if let Err(err) = std::fs::remove_dir_all(root) {
            tracing::warn!("could not remove {}: {err}", root.display());
        }
    }

    /// The expected argument list, as text.
    fn os_args(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(Into::into).collect()
    }

    #[test]
    fn the_client_exes_bundle_is_the_app_three_folders_up() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/rustling-tulip.app/Contents/MacOS/rustling-tulip-native"
            )),
            Some(PathBuf::from("/Applications/rustling-tulip.app"))
        );
        assert_eq!(
            bundle_of(Path::new("target/test-release/rustling-tulip-native")),
            None
        );
        assert_eq!(
            bundle_of(Path::new(
                r"C:\Program Files\rustling-tulip\rustling-tulip-native.exe"
            )),
            None
        );
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/rustling-tulip/Contents/MacOS/rustling-tulip-native"
            )),
            None
        );
    }

    #[test]
    fn the_hand_off_runs_apply_update_on_the_staged_app() {
        let staged = Path::new(
            "/Applications/.rustling-tulip-update-0.9.1-alpha-0123abcd/rustling-tulip.app",
        );
        let installed = Path::new("/Applications/rustling-tulip.app");

        let args = apply_update_args(staged, installed, "0.9.1-alpha", false, true);

        let expected: Vec<std::ffi::OsString> = [
            "apply-update",
            "--from",
            "/Applications/.rustling-tulip-update-0.9.1-alpha-0123abcd/rustling-tulip.app",
            "--to",
            "/Applications/rustling-tulip.app",
            "--old-version",
            "0.9.1-alpha",
            "--update",
            "--relaunch",
        ]
        .iter()
        .map(Into::into)
        .collect();
        assert_eq!(args, expected);
    }

    #[test]
    fn apply_update_args_adds_restart_daemon_and_relaunch_as_asked() {
        let staged = Path::new("/Applications/.rustling-tulip-update-0.99.0-abcd1234/x.app");
        let installed = Path::new("/Applications/rustling-tulip.app");
        let base = [
            "apply-update",
            "--from",
            "/Applications/.rustling-tulip-update-0.99.0-abcd1234/x.app",
            "--to",
            "/Applications/rustling-tulip.app",
            "--old-version",
            "0.9.1",
            "--update",
        ];

        let restart = apply_update_args(staged, installed, "0.9.1", true, false);
        let both = apply_update_args(staged, installed, "0.9.1", true, true);

        let mut restart_expected = base.to_vec();
        restart_expected.push("--restart-daemon");
        assert_eq!(restart, os_args(&restart_expected));
        let mut both_expected = restart_expected.clone();
        both_expected.push("--relaunch");
        assert_eq!(both, os_args(&both_expected));
    }

    /// `folder`, with its own name kept at the zip's top level, zipped at `zip`.
    #[cfg(target_os = "macos")]
    fn zipped(folder: &Path, zip: &Path) {
        let status = std::process::Command::new("/usr/bin/ditto")
            .arg("-c")
            .arg("-k")
            .arg("--keepParent")
            .arg(folder)
            .arg(zip)
            .status()
            .expect("run ditto");
        assert!(status.success(), "ditto failed: {status}");
    }

    /// Every staging folder in `folder`.
    #[cfg(target_os = "macos")]
    fn staging_folders(folder: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(folder)
            .expect("list the install folder")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(STAGING_PREFIX))
            })
            .collect()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn run_bounded_kills_a_child_that_overruns_its_bound() {
        let mut command = std::process::Command::new("/bin/sleep");
        command.arg("30");

        let err =
            run_bounded(&mut command, Duration::from_secs(1)).expect_err("a child past its bound");

        assert!(err.to_string().contains("did not finish"), "{err:#}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn run_bounded_reports_the_childs_outcome() {
        let mut ok = std::process::Command::new("/usr/bin/true");
        run_bounded(&mut ok, HANG_GUARD).expect("a successful child succeeds");
        let mut bad = std::process::Command::new("/usr/bin/false");

        assert!(
            run_bounded(&mut bad, HANG_GUARD).is_err(),
            "a failing child fails the run"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_zip_without_the_app_is_refused_and_leaves_no_staging_folder() {
        let root = scratch("mac-no-app");
        let junk = root.join("junk");
        std::fs::create_dir_all(&junk).expect("a folder to zip");
        std::fs::write(junk.join("file"), b"junk").expect("a file to zip");
        let zip = root.join("bad.zip");
        zipped(&junk, &zip);
        let install = root.join("Applications");
        let installed = install.join(BUNDLE_NAME);
        std::fs::create_dir_all(&installed).expect("the installed bundle");

        let err = stage(&zip, &installed, &v("0.9.1-alpha"))
            .expect_err("a zip without the app cannot be staged");

        assert!(
            format!("{err:#}").contains("holds no rustling-tulip.app"),
            "{err:#}"
        );
        let left = staging_folders(&install);
        assert!(left.is_empty(), "left behind: {left:?}");
        remove(&root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_translocated_bundle_is_refused_before_anything_is_staged() {
        let root = scratch("mac-translocated");
        let install = root.join("AppTranslocation");
        let installed = install.join(BUNDLE_NAME);
        std::fs::create_dir_all(&installed).expect("the installed bundle");
        let zip = root.join("u.zip");
        std::fs::write(&zip, b"never read").expect("the zip's place");

        let err = stage(&zip, &installed, &v("0.9.1-alpha"))
            .expect_err("a translocated bundle cannot be replaced");

        assert_eq!(format!("{err:#}"), UPDATE_REFUSED);
        let staged = staging_folders(&install);
        assert!(staged.is_empty(), "staged anyway: {staged:?}");
        assert!(
            !install.join(UPDATE_PROBE).exists(),
            "the probe file stayed"
        );
        remove(&root);
    }
}
