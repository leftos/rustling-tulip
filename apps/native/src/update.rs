//! The updater's client half: the [`Updater`] seam the window checks and
//! installs through, [`SystemUpdater`] against the real updates folder and
//! installer, the platform's installer launch and macOS bundle swap, and
//! launch housekeeping. The feed, download and verify steps the daemon also
//! needs live in `update-core`, re-exported here.

use anyhow::{Context as _, anyhow};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[cfg(target_os = "macos")]
use anyhow::bail;
#[cfg(target_os = "macos")]
use std::time::Duration;

pub use update_core::{
    DEFAULT_FEED_BASE, FEED_BASE_VAR, HASH_MISMATCH, LastResult, Offer, Platform, Ready,
    ReleaseNotes, UPDATE_MUTEX, asset_name, changelog_text, check_and_download, clean_old,
    feed_base, fetch_changelog, parse_changelog, parse_releases, parse_releases_for, parse_sha256,
    take_last_result,
};

/// The prefix of the hidden folder a macOS update unpacks the downloaded app
/// into, beside the installed bundle: the next launch removes it.
const STAGING_PREFIX: &str = ".rustling-tulip-update-";

/// The arguments the installer runs with for an update.
#[cfg(windows)]
const INSTALLER_ARGS: [&str; 3] = ["/S", "/UPDATE", "/RELAUNCH"];

/// What a start fails with while another update holds [`UPDATE_MUTEX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyRunning;

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("another update holds the update lock")
    }
}

impl std::error::Error for AlreadyRunning {}

/// What the client's window checks, installs and reads back through, so the
/// specs replace it.
pub trait Updater {
    /// Whether this build checks the feed and records the version it runs.
    /// An inactive build neither announces an install nor saves its version,
    /// so a dev build over the installed client's layout leaves the saved
    /// version alone.
    fn is_active(&self) -> bool;

    /// Checks the feed for a release newer than `current` and, when there is
    /// one, downloads and verifies it.
    fn check(
        &self,
        current: &semver::Version,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Option<Ready>>>;

    /// The changelog for the releases after `after` and up to `through`, for
    /// the What's new button the update notices show.
    ///
    /// # Errors
    ///
    /// Fails when the feed cannot be fetched or decoded, or in a build that
    /// does not check for updates.
    fn changelog(
        &self,
        after: Option<semver::Version>,
        through: semver::Version,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<String>>;

    /// Starts `ready`'s installer for an update.
    ///
    /// # Errors
    ///
    /// [`AlreadyRunning`] while another update holds the update lock, or why
    /// the installer did not start.
    fn start_installer(&self, ready: &Ready) -> anyhow::Result<()>;

    /// The outcome the last update recorded, read once: the file is deleted.
    fn read_last_result(&self) -> Option<LastResult>;
}

/// Whether this build checks the feed: a build without debug assertions
/// always does, a dev build only with [`FEED_BASE_VAR`] set.
#[must_use]
pub fn checks_allowed() -> bool {
    !cfg!(debug_assertions) || std::env::var_os(FEED_BASE_VAR).is_some_and(|v| !v.is_empty())
}

/// The updater against the real feed, the real updates folder and a real
/// installer launch.
pub struct SystemUpdater {
    /// The updates folder, or why it could not be resolved.
    updates_root: Result<PathBuf, String>,
    /// Whether checks and the last result are this client's: off in a dev
    /// build without [`FEED_BASE_VAR`] and in the cloaked smoke window.
    active: bool,
}

impl SystemUpdater {
    /// The updater over `updates_root`; an inactive one never checks and
    /// reads no last result.
    #[must_use]
    pub fn new(updates_root: Result<PathBuf, String>, active: bool) -> Self {
        Self {
            updates_root,
            active,
        }
    }
}

impl Updater for SystemUpdater {
    fn is_active(&self) -> bool {
        self.active
    }

    fn check(
        &self,
        current: &semver::Version,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Option<Ready>>> {
        use futures::FutureExt as _;
        if !self.active {
            return futures::future::ready(Err(anyhow!(
                "this build does not check for updates (a dev build needs {FEED_BASE_VAR})"
            )))
            .boxed();
        }
        let root = match &self.updates_root {
            Ok(root) => root.clone(),
            Err(err) => {
                return futures::future::ready(Err(anyhow!("{err}"))).boxed();
            }
        };
        let (tx, rx) = futures::channel::oneshot::channel();
        let current = current.clone();
        let spawned = std::thread::Builder::new()
            .name("update-check".to_owned())
            .spawn(move || {
                // The receiver is gone only when the window closed.
                let _ = tx.send(check_and_download(&current, &root, &feed_base()));
            });
        async move {
            spawned.context("spawning the update check")?;
            rx.await
                .map_err(|_| anyhow!("the update check ended without an answer"))?
        }
        .boxed()
    }

    fn changelog(
        &self,
        after: Option<semver::Version>,
        through: semver::Version,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<String>> {
        use futures::FutureExt as _;
        if !self.active {
            return futures::future::ready(Err(anyhow!(
                "this build does not check for updates (a dev build needs {FEED_BASE_VAR})"
            )))
            .boxed();
        }
        let (tx, rx) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("update-changelog".to_owned())
            .spawn(move || {
                // The receiver is gone only when the window closed.
                let _ = tx.send(fetch_changelog(after.as_ref(), &through, &feed_base()));
            });
        async move {
            spawned.context("spawning the changelog fetch")?;
            rx.await
                .map_err(|_| anyhow!("the changelog fetch ended without an answer"))?
        }
        .boxed()
    }

    fn start_installer(&self, ready: &Ready) -> anyhow::Result<()> {
        let hold = update_core::try_lock(UPDATE_MUTEX)?.ok_or(AlreadyRunning)?;
        start_installer_on_this_platform(ready)?;
        // The installer's `apply-update` takes the lock next.
        drop(hold);
        Ok(())
    }

    fn read_last_result(&self) -> Option<LastResult> {
        if !self.active {
            return None;
        }
        let root = self.updates_root.as_ref().ok()?;
        take_last_result(&root.join("last-result.json"))
    }
}

/// Starts the downloaded update on Windows: the NSIS installer in its update
/// mode, which runs the staged `apply-update` itself.
#[cfg(windows)]
fn start_installer_on_this_platform(ready: &Ready) -> anyhow::Result<()> {
    let args: Vec<std::ffi::OsString> = INSTALLER_ARGS.iter().map(Into::into).collect();
    update_core::launch_detached(&ready.path, &args)?;
    info!(path = %ready.path.display(), "update: installer started");
    Ok(())
}

/// Starts the downloaded update on macOS: the zip is unpacked beside the
/// installed bundle, and the staged app's `apply-update` swaps it in.
#[cfg(target_os = "macos")]
fn start_installer_on_this_platform(ready: &Ready) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("finding the client exe")?;
    let bundle = bundle_of(&exe).ok_or_else(|| {
        anyhow!(
            "rustling-tulip-native is not running from a rustling-tulip.app bundle; \
             update by installing the new release"
        )
    })?;
    start_macos_update(ready, &bundle)
}

/// Never reached: a client on a platform the feed publishes no asset for
/// holds no [`Ready`] to install.
#[cfg(not(any(windows, target_os = "macos")))]
fn start_installer_on_this_platform(_ready: &Ready) -> anyhow::Result<()> {
    Err(anyhow!("this platform has no updater"))
}

/// Launch housekeeping: deletes the old downloads under `updates_root`, the
/// `*.old` files an update left beside the client exe, and, for a client
/// running from an app bundle, the replaced app and staging folder a bundle
/// update left beside it. Best-effort; each failure is logged at warn.
pub fn clean_at_launch(updates_root: anyhow::Result<PathBuf>) {
    match updates_root {
        Ok(root) => clean_old(&root, None),
        Err(err) => warn!("update: no updates folder to clean: {err:#}"),
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            warn!("update: could not find the client exe: {err}");
            return;
        }
    };
    if let Some(folder) = exe.parent() {
        remove_old_files(folder);
    } else {
        warn!("update: the client exe has no folder");
    }
    if let Some(bundle) = bundle_of(&exe) {
        remove_bundle_leftovers(&bundle);
    }
}

/// The app bundle `exe` runs from, for a client at
/// `<bundle>/Contents/MacOS/<exe>`; `None` for any other layout. Pure path
/// logic: nothing is read from disk and nothing is canonicalized, so a
/// translocated bundle's path stays as the system gave it.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
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

/// Deletes what an earlier bundle update left beside `bundle`: the replaced
/// `{bundle}.old` folder and any [`STAGING_PREFIX`] folder. Everything else in
/// the folder is left alone; each failure is logged at warn and the walk goes
/// on.
fn remove_bundle_leftovers(bundle: &Path) {
    let (Some(folder), Some(name)) = (bundle.parent(), bundle.file_name()) else {
        return;
    };
    let replaced = format!("{}.old", name.to_string_lossy());
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(folder = %folder.display(), "update: could not list the app's folder: {err}");
            return;
        }
    };
    for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        let leftover = path.file_name().is_some_and(|entry| {
            let entry = entry.to_string_lossy();
            entry == replaced || entry.starts_with(STAGING_PREFIX)
        });
        if !leftover || !path.is_dir() {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => info!(path = %path.display(), "update: deleted what an update left behind"),
            Err(err) => {
                warn!(path = %path.display(), "update: could not delete what an update left behind: {err}");
            }
        }
    }
}

/// Deletes every `*.old` file directly in `folder`.
fn remove_old_files(folder: &Path) {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(folder = %folder.display(), "update: could not list the install folder: {err}");
            return;
        }
    };
    for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        if path.is_file() && path.extension().is_some_and(|ext| ext == "old") {
            match std::fs::remove_file(&path) {
                Ok(()) => info!(path = %path.display(), "update: deleted a replaced file"),
                Err(err) => {
                    warn!(path = %path.display(), "update: could not delete a replaced file: {err}");
                }
            }
        }
    }
}

/// The version this client reports to `apply-update` as the one it replaces.
#[cfg(target_os = "macos")]
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The system's zip tool: it keeps the bundle's symlinks and permissions.
#[cfg(target_os = "macos")]
const DITTO: &str = "/usr/bin/ditto";

/// A bound on unpacking the downloaded bundle.
#[cfg(target_os = "macos")]
const DITTO_TIMEOUT: Duration = Duration::from_secs(120);

/// The folder the downloaded zip holds at its top level.
#[cfg(target_os = "macos")]
const BUNDLE_NAME: &str = "rustling-tulip.app";

/// The probe file a start writes into the bundle's folder and removes again.
#[cfg(target_os = "macos")]
const UPDATE_PROBE: &str = ".rustling-tulip-update-probe";

/// What a start refuses with when the bundle's place cannot be replaced.
#[cfg(target_os = "macos")]
const UPDATE_REFUSED: &str = "rustling-tulip.app runs from a place the update cannot replace; \
                              move it to Applications and update again; nothing was changed";

/// Stages `ready`'s zip beside `bundle` and hands the swap to the staged
/// daemon's `apply-update`, which waits for this client to exit. Returns as
/// soon as the hand-off started: anything staged is removed on failure, and
/// what a successful hand-off leaves is removed by [`clean_at_launch`].
#[cfg(target_os = "macos")]
fn start_macos_update(ready: &Ready, bundle: &Path) -> anyhow::Result<()> {
    check_update_place(bundle)?;
    let staging = fresh_staging_folder(bundle, &ready.version)?;
    match stage_update(ready, bundle, &staging) {
        Ok(()) => {
            info!(
                version = %ready.version,
                bundle = %bundle.display(),
                "update: handed over to apply-update"
            );
            Ok(())
        }
        Err(err) => {
            if let Err(cleanup) = std::fs::remove_dir_all(&staging) {
                warn!(
                    folder = %staging.display(),
                    "update: could not delete the staging folder: {cleanup}"
                );
            }
            Err(err)
        }
    }
}

/// Refuses a bundle the update cannot replace: one the system runs
/// translocated, or one whose folder this process cannot write a probe file
/// in.
#[cfg(target_os = "macos")]
fn check_update_place(bundle: &Path) -> anyhow::Result<()> {
    let translocated = bundle
        .components()
        .any(|part| part.as_os_str() == std::ffi::OsStr::new("AppTranslocation"));
    if translocated {
        bail!("{UPDATE_REFUSED}");
    }
    let Some(folder) = bundle.parent() else {
        bail!("{UPDATE_REFUSED}");
    };
    let probe = folder.join(UPDATE_PROBE);
    let writable = match std::fs::write(&probe, b"") {
        Ok(()) => true,
        Err(err) => {
            warn!(
                path = %probe.display(),
                "update: cannot write beside the app bundle: {err}"
            );
            false
        }
    };
    if let Err(err) = std::fs::remove_file(&probe) {
        warn!(path = %probe.display(), "update: could not remove the probe file: {err}");
    }
    if writable {
        Ok(())
    } else {
        bail!("{UPDATE_REFUSED}")
    }
}

/// A new, empty staging folder beside `bundle`, on the same volume so
/// `apply-update` can rename the staged app into place.
#[cfg(target_os = "macos")]
fn fresh_staging_folder(bundle: &Path, version: &semver::Version) -> anyhow::Result<PathBuf> {
    let folder = bundle
        .parent()
        .ok_or_else(|| anyhow!("{} is not inside a folder", bundle.display()))?;
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
    bail!("could not create a staging folder in {}", folder.display())
}

/// Unpacks `ready`'s zip into `staging` and starts the staged daemon's
/// `apply-update` on the swap.
#[cfg(target_os = "macos")]
fn stage_update(ready: &Ready, bundle: &Path, staging: &Path) -> anyhow::Result<()> {
    let mut ditto = std::process::Command::new(DITTO);
    ditto.arg("-x").arg("-k").arg(&ready.path).arg(staging);
    run_bounded(&mut ditto, DITTO_TIMEOUT).context("unpacking the update")?;
    let staged_app = staging.join(BUNDLE_NAME);
    let staged_daemon = staged_app.join("Contents/MacOS/rustling-tulipd");
    if !staged_daemon.is_file() {
        bail!("the update archive holds no rustling-tulip.app");
    }
    update_core::launch_detached(
        &staged_daemon,
        &apply_update_args(&staged_app, bundle, CLIENT_VERSION),
    )
}

/// The arguments the staged daemon runs: swap `staged_app` for `bundle`, and
/// relaunch the client once this one has exited.
#[cfg(any(test, target_os = "macos"))]
fn apply_update_args(staged_app: &Path, bundle: &Path, version: &str) -> Vec<std::ffi::OsString> {
    vec![
        "apply-update".into(),
        "--from".into(),
        staged_app.into(),
        "--to".into(),
        bundle.into(),
        "--old-version".into(),
        version.into(),
        "--update".into(),
        "--relaunch".into(),
    ]
}

/// Runs `command` to its own exit, killing it once `timeout` passes.
#[cfg(target_os = "macos")]
fn run_bounded(command: &mut std::process::Command, timeout: Duration) -> anyhow::Result<()> {
    const POLL: Duration = Duration::from_millis(20);
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
            Ok(Some(status)) => bail!("{program} failed: {status}"),
            Ok(None) => {}
            Err(err) => {
                stop_child(&mut child, &program);
                return Err(err).with_context(|| format!("waiting on {program}"));
            }
        }
        if started.elapsed() >= timeout {
            stop_child(&mut child, &program);
            bail!("{program} did not finish within {} s", timeout.as_secs());
        }
        std::thread::sleep(POLL);
    }
}

/// Kills `child`, logging a kill that fails; its exit is then reaped.
#[cfg(target_os = "macos")]
fn stop_child(child: &mut std::process::Child, program: &str) {
    if let Err(err) = child.kill() {
        warn!("update: could not stop {program}: {err}");
    }
    if let Err(err) = child.wait() {
        warn!("update: could not reap {program}: {err}");
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::{
        BUNDLE_NAME, CLIENT_VERSION, Ready, UPDATE_PROBE, UPDATE_REFUSED, run_bounded,
        start_macos_update,
    };
    use super::{STAGING_PREFIX, apply_update_args, bundle_of, remove_bundle_leftovers};
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

    /// A fresh updates root, not yet created.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rt-update-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn remove(root: &Path) {
        if let Err(err) = std::fs::remove_dir_all(root) {
            tracing::warn!("could not remove {}: {err}", root.display());
        }
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

        let args = apply_update_args(staged, installed, "0.9.1-alpha");

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
    fn what_an_update_left_beside_the_bundle_is_removed() {
        let root = scratch("leftovers");
        let bundle = root.join("x.app");
        std::fs::create_dir_all(bundle.join("Contents")).expect("the bundle");
        std::fs::write(bundle.join("Contents/Info.plist"), b"plist").expect("a file in the bundle");
        let replaced = root.join("x.app.old");
        std::fs::create_dir_all(&replaced).expect("the replaced app");
        std::fs::write(replaced.join("client"), b"old").expect("a file in the replaced app");
        let staging = root.join(format!("{STAGING_PREFIX}0.9.1-alpha-0123abcd"));
        std::fs::create_dir_all(&staging).expect("a staging folder");
        let other = root.join("other.app.old");
        std::fs::create_dir_all(&other).expect("an unrelated replaced app");
        std::fs::write(root.join("keep.txt"), b"keep").expect("an unrelated file");

        remove_bundle_leftovers(&bundle);

        assert!(
            bundle.join("Contents/Info.plist").is_file(),
            "the bundle's own file went"
        );
        assert!(other.is_dir(), "an unrelated .app.old folder went");
        assert!(root.join("keep.txt").is_file(), "an unrelated file went");
        assert!(!replaced.exists(), "the replaced app is still there");
        assert!(!staging.exists(), "the staging folder is still there");
        remove(&root);
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

    /// A `rustling-tulip.app` at `app` whose `rustling-tulipd` is a shell
    /// script writing its arguments, one per line, to `args_path`.
    #[cfg(target_os = "macos")]
    fn app_with_script_daemon(app: &Path, args_path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let executables = app.join("Contents").join("MacOS");
        std::fs::create_dir_all(&executables).expect("the bundle's executables folder");
        let script = executables.join("rustling-tulipd");
        let partial = args_path.with_extension("tmp");
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{partial}' && mv '{partial}' '{whole}'\n",
            partial = partial.display(),
            whole = args_path.display()
        );
        std::fs::write(&script, body).expect("the staged daemon script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("an executable staged daemon");
    }

    /// The arguments the staged daemon was handed, once the script wrote them
    /// out whole; bounded only by [`HANG_GUARD`].
    #[cfg(target_os = "macos")]
    fn handed_arguments(args_path: &Path) -> String {
        let started = std::time::Instant::now();
        loop {
            if let Ok(text) = std::fs::read_to_string(args_path)
                && text.ends_with('\n')
            {
                return text;
            }
            assert!(
                started.elapsed() < HANG_GUARD,
                "hung waiting for {}",
                args_path.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
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

    /// The one staging folder in `folder`.
    #[cfg(target_os = "macos")]
    fn only_staging(folder: &Path) -> PathBuf {
        let found = staging_folders(folder);
        assert_eq!(found.len(), 1, "staging folders: {found:?}");
        found.into_iter().next().expect("one staging folder")
    }

    /// A [`Ready`] for the zip at `zip`.
    #[cfg(target_os = "macos")]
    fn ready_at(zip: &Path) -> Ready {
        Ready {
            version: v("0.9.1-alpha"),
            path: zip.to_path_buf(),
            file: std::fs::File::open(zip).expect("open the downloaded zip"),
            notes: String::new(),
        }
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
    fn a_mac_update_stages_the_app_and_hands_over() {
        let root = scratch("mac-hand-off");
        let source = root.join("src").join(BUNDLE_NAME);
        let args_path = root.join("args.txt");
        app_with_script_daemon(&source, &args_path);
        let zip = root.join("u.zip");
        zipped(&source, &zip);
        let install = root.join("Applications");
        let installed = install.join(BUNDLE_NAME);
        std::fs::create_dir_all(&installed).expect("the installed bundle");
        let ready = ready_at(&zip);

        start_macos_update(&ready, &installed).expect("the update is handed over");

        let got = handed_arguments(&args_path);
        assert_eq!(got.lines().count(), 9, "{got}");
        let staging = only_staging(&install);
        assert!(
            staging.join(BUNDLE_NAME).is_dir(),
            "the staged app is not there"
        );
        let expected: Vec<String> =
            apply_update_args(&staging.join(BUNDLE_NAME), &installed, CLIENT_VERSION)
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
        assert_eq!(got, format!("{}\n", expected.join("\n")));
        remove(&root);
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
        let ready = ready_at(&zip);

        let err = start_macos_update(&ready, &installed)
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
        let ready = ready_at(&zip);

        let err = start_macos_update(&ready, &installed)
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
