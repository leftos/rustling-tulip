//! The updater's client half: the [`Updater`] seam the window checks and
//! installs through, [`SystemUpdater`] against the real updates folder and
//! installer, the platform's installer launch and macOS bundle swap, and
//! launch housekeeping. The feed, download and verify steps the daemon also
//! needs live in `update-core`, re-exported here.

use anyhow::{Context as _, anyhow};
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use update_core::macos_stage::{STAGING_PREFIX, bundle_of};

pub use update_core::{
    Breaks, BreaksOffer, DEFAULT_FEED_BASE, FEED_BASE_VAR, HASH_MISMATCH, LastResult, Offer,
    Platform, Ready, ReleaseNotes, UPDATE_MUTEX, asset_name, breaks_of, breaks_since,
    changelog_text, check_and_download, checks_allowed, clean_old, feed_base, fetch_changelog,
    parse_changelog, parse_releases, parse_releases_for, parse_sha256, take_last_result,
};

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

    /// What the releases after `from` up to the newest break, read from the
    /// feed, and the newest version; `None` when nothing is newer.
    ///
    /// # Errors
    ///
    /// Fails when the feed cannot be fetched or decoded, or in a build that
    /// does not check for updates.
    fn breaks_since(
        &self,
        from: semver::Version,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Option<BreaksOffer>>>;

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

    fn breaks_since(
        &self,
        from: semver::Version,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Option<BreaksOffer>>> {
        use futures::FutureExt as _;
        if !self.active {
            return futures::future::ready(Err(anyhow!(
                "this build does not check for updates (a dev build needs {FEED_BASE_VAR})"
            )))
            .boxed();
        }
        let (tx, rx) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("update-breaks".to_owned())
            .spawn(move || {
                // The receiver is gone only when the window closed.
                let _ = tx.send(breaks_since(&from, &feed_base()));
            });
        async move {
            spawned.context("spawning the release check")?;
            rx.await
                .map_err(|_| anyhow!("the release check ended without an answer"))?
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

/// Stages `ready`'s zip beside `bundle` and hands the swap to the staged
/// daemon's `apply-update`, which waits for this client to exit. Returns as
/// soon as the hand-off started: anything staged is removed on failure, and
/// what a successful hand-off leaves is removed by [`clean_at_launch`].
#[cfg(target_os = "macos")]
fn start_macos_update(ready: &Ready, bundle: &Path) -> anyhow::Result<()> {
    use update_core::macos_stage;
    let staged_app = macos_stage::stage(&ready.path, bundle, &ready.version)?;
    let args = macos_stage::apply_update_args(&staged_app, bundle, CLIENT_VERSION, false, true);
    if let Err(err) = update_core::launch_detached(&macos_stage::staged_daemon(&staged_app), &args)
    {
        if let Some(staging) = staged_app.parent() {
            macos_stage::remove_staging(staging);
        }
        return Err(err);
    }
    info!(
        version = %ready.version,
        bundle = %bundle.display(),
        "update: handed over to apply-update"
    );
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::{CLIENT_VERSION, Ready, start_macos_update};
    use super::{STAGING_PREFIX, remove_bundle_leftovers};
    use std::path::{Path, PathBuf};
    #[cfg(target_os = "macos")]
    use std::time::Duration;
    #[cfg(target_os = "macos")]
    use update_core::macos_stage::{BUNDLE_NAME, apply_update_args};

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
        let expected: Vec<String> = apply_update_args(
            &staging.join(BUNDLE_NAME),
            &installed,
            CLIENT_VERSION,
            false,
            true,
        )
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
        assert_eq!(got, format!("{}\n", expected.join("\n")));
        remove(&root);
    }
}
