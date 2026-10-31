//! The updater's core: finds a newer published release on the GitHub Releases
//! feed, downloads its installer into a fresh folder, and verifies it against
//! the release's `.sha256` asset while holding the file open so nothing can
//! replace it (`docs/auto-update.md`, decisions 1, 2 and 4). No UI lives
//! here: the client calls [`check_and_download`] from a background task.

use anyhow::{Context as _, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tracing::{info, warn};

/// The release feed's base URL when [`FEED_BASE_VAR`] is unset.
pub const DEFAULT_FEED_BASE: &str = "https://api.github.com";
/// The variable that points the release feed at another base URL.
pub const FEED_BASE_VAR: &str = "RUSTLING_TULIP_UPDATE_FEED";
/// What a download whose SHA-256 differs from its release's fails with.
pub const HASH_MISMATCH: &str = "the download's SHA-256 does not match its release";

/// The feed's path below its base URL.
const FEED_PATH: &str = "/repos/leftos/rustling-tulip/releases";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A bound on one whole request, the installer's download included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// A release newer than the running client that the feed offers, with both
/// its assets' download URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// The release's version, from its `v<version>` tag.
    pub version: semver::Version,
    /// Where `rustling-tulip-<version>-setup.exe` downloads from.
    pub installer_url: String,
    /// Where `rustling-tulip-<version>-setup.exe.sha256` downloads from.
    pub sha256_url: String,
    /// `rustling-tulip-<version>-setup.exe`.
    pub installer_name: String,
}

/// A downloaded installer whose SHA-256 matched its release. `file` is held
/// open with write and delete sharing denied, so the verified file cannot be
/// replaced for as long as this lives.
#[derive(Debug)]
pub struct Ready {
    /// The release's version.
    pub version: semver::Version,
    /// The installer, in its own `updates/<version>-<random>/` folder.
    pub path: PathBuf,
    /// The handle the hash was read through.
    pub file: File,
}

/// One release in the feed; only the fields the pick reads.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// The release feed's base URL: [`FEED_BASE_VAR`] when set and non-empty,
/// else [`DEFAULT_FEED_BASE`].
#[must_use]
pub fn feed_base() -> String {
    match std::env::var(FEED_BASE_VAR) {
        Ok(value) if !value.is_empty() => value,
        _ => DEFAULT_FEED_BASE.to_owned(),
    }
}

/// The newest non-draft release in the feed `json` strictly newer than
/// `current` by semver, pre-releases included, that carries both its
/// installer and its `.sha256` asset. A release whose tag is not
/// `v<version>` is skipped; a feed that does not decode is logged and offers
/// nothing.
#[must_use]
pub fn parse_releases(json: &str, current: &semver::Version) -> Option<Offer> {
    match decode_releases(json) {
        Ok(releases) => pick(&releases, current),
        Err(err) => {
            warn!("update: {err:#}");
            None
        }
    }
}

fn decode_releases(json: &str) -> anyhow::Result<Vec<Release>> {
    serde_json::from_str(json).context("the release feed did not decode")
}

/// See [`parse_releases`].
fn pick(releases: &[Release], current: &semver::Version) -> Option<Offer> {
    releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(offer_of)
        .filter(|offer| offer.version > *current)
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// The offer `release` makes: its tag `v<version>` parsed, and both assets
/// found by name.
fn offer_of(release: &Release) -> Option<Offer> {
    let raw = release.tag_name.strip_prefix('v')?;
    let version = semver::Version::parse(raw).ok()?;
    let installer_name = format!("rustling-tulip-{raw}-setup.exe");
    let sha256_name = format!("{installer_name}.sha256");
    let url_of = |name: &str| {
        release
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .map(|asset| asset.browser_download_url.clone())
    };
    Some(Offer {
        version,
        installer_url: url_of(&installer_name)?,
        sha256_url: url_of(&sha256_name)?,
        installer_name,
    })
}

/// The SHA-256 a release's `.sha256` asset holds: exactly
/// `<64 hex>  <expected_name>`, with optional trailing whitespace or newline.
///
/// # Errors
///
/// Fails when the text is not that shape, names another file, or its hash is
/// not 64 hex digits.
pub fn parse_sha256(text: &str, expected_name: &str) -> anyhow::Result<[u8; 32]> {
    let line = text.trim_end();
    let (hex, name) = line
        .split_once("  ")
        .ok_or_else(|| anyhow!("the .sha256 file is not `<hash>  <name>`: {line:?}"))?;
    if name != expected_name {
        bail!("the .sha256 file names {name:?}, not {expected_name:?}");
    }
    decode_hex(hex)
}

/// `hex` as 32 bytes, refusing anything but 64 hex digits.
fn decode_hex(hex: &str) -> anyhow::Result<[u8; 32]> {
    let digits: Vec<u8> = hex
        .chars()
        .map(|c| c.to_digit(16).and_then(|digit| u8::try_from(digit).ok()))
        .collect::<Option<_>>()
        .ok_or_else(|| anyhow!("the .sha256 hash is not hex: {hex:?}"))?;
    if digits.len() != 64 {
        bail!("the .sha256 hash has {} hex digits, not 64", digits.len());
    }
    let mut bytes = [0_u8; 32];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
        *byte = high * 16 + low;
    }
    Ok(bytes)
}

/// Checks the feed at `feed_base` for a release newer than `current` and, when
/// there is one, downloads and verifies it into a fresh folder under
/// `updates_root`. The network calls run on a thread of their own with a
/// current-thread runtime; this blocks until they finish, so call it from a
/// background task.
///
/// # Errors
///
/// Fails when the feed cannot be fetched or decoded, a download fails, or the
/// download's SHA-256 does not match its release ([`HASH_MISMATCH`]); a
/// failed download's folder is deleted.
pub fn check_and_download(
    current: &semver::Version,
    updates_root: &Path,
    feed_base: &str,
) -> anyhow::Result<Option<Ready>> {
    let (current, updates_root, feed_base) = (
        current.clone(),
        updates_root.to_path_buf(),
        feed_base.to_owned(),
    );
    let worker = std::thread::Builder::new()
        .name("update".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("building the update thread's runtime")?;
            runtime.block_on(Box::pin(check_then_download(
                &current,
                &updates_root,
                &feed_base,
            )))
        })
        .context("spawning the update thread")?;
    worker
        .join()
        .map_err(|_| anyhow!("the update thread panicked"))?
}

async fn check_then_download(
    current: &semver::Version,
    updates_root: &Path,
    feed_base: &str,
) -> anyhow::Result<Option<Ready>> {
    let http = http_client(current)?;
    let Some(offer) = fetch_offer(&http, current, feed_base).await? else {
        info!(%current, "update: no newer release");
        return Ok(None);
    };
    info!(version = %offer.version, "update: downloading release");
    download(&http, &offer, updates_root).await.map(Some)
}

fn http_client(current: &semver::Version) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("rustling-tulip/{current}"))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building the update HTTP client")
}

async fn fetch_offer(
    http: &reqwest::Client,
    current: &semver::Version,
    feed_base: &str,
) -> anyhow::Result<Option<Offer>> {
    let url = format!("{}{FEED_PATH}", feed_base.trim_end_matches('/'));
    let body = http
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("fetching the release feed {url}"))?
        .text()
        .await
        .with_context(|| format!("reading the release feed {url}"))?;
    Ok(pick(&decode_releases(&body)?, current))
}

/// Downloads `offer` into a fresh `updates_root/<version>-<8 random hex>`
/// folder: the `.sha256` first, then the installer, which is verified through
/// a held handle. Any failure deletes the folder.
async fn download(
    http: &reqwest::Client,
    offer: &Offer,
    updates_root: &Path,
) -> anyhow::Result<Ready> {
    let folder = fresh_folder(updates_root, &offer.version)?;
    let fetched = fetch_and_verify(http, offer, &folder).await;
    if fetched.is_err()
        && let Err(err) = std::fs::remove_dir_all(&folder)
    {
        warn!(folder = %folder.display(), "update: could not delete a failed download: {err}");
    }
    fetched
}

fn fresh_folder(updates_root: &Path, version: &semver::Version) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(updates_root)
        .with_context(|| format!("creating {}", updates_root.display()))?;
    let random: String = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect();
    let folder = updates_root.join(format!("{version}-{random}"));
    std::fs::create_dir(&folder).with_context(|| format!("creating {}", folder.display()))?;
    Ok(folder)
}

async fn fetch_and_verify(
    http: &reqwest::Client,
    offer: &Offer,
    folder: &Path,
) -> anyhow::Result<Ready> {
    let sha256_text = get(http, &offer.sha256_url)
        .await?
        .text()
        .await
        .with_context(|| format!("reading {}", offer.sha256_url))?;
    let expected = parse_sha256(&sha256_text, &offer.installer_name)?;
    let path = folder.join(&offer.installer_name);
    fetch_to_file(http, &offer.installer_url, &path).await?;
    let mut file =
        open_held(&path).with_context(|| format!("opening {} to verify it", path.display()))?;
    let actual = sha256_of(&mut file).with_context(|| format!("hashing {}", path.display()))?;
    if actual != expected {
        warn!(version = %offer.version, "update: the download's SHA-256 does not match its release; discarded");
        bail!("{HASH_MISMATCH}");
    }
    info!(version = %offer.version, path = %path.display(), "update: release downloaded and verified");
    Ok(Ready {
        version: offer.version.clone(),
        path,
        file,
    })
}

async fn get(http: &reqwest::Client, url: &str) -> anyhow::Result<reqwest::Response> {
    http.get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("fetching {url}"))
}

/// Streams `url` into a new file at `path`, closed when this returns.
async fn fetch_to_file(http: &reqwest::Client, url: &str, path: &Path) -> anyhow::Result<()> {
    let mut response = get(http, url).await?;
    let mut file = tokio::fs::File::create(path)
        .await
        .with_context(|| format!("creating {}", path.display()))?;
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("downloading {url}"))?
    {
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
    }
    file.flush()
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    drop(file.into_std().await);
    Ok(())
}

/// Opens `path` for reading with write and delete sharing denied (Windows),
/// so nothing can write, rename over or delete it while the handle lives.
fn open_held(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0);
    }
    options.open(path)
}

fn sha256_of(file: &mut File) -> std::io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..read]);
    }
}

/// Deletes every folder under `updates_root` but `keep` (a download folder,
/// such as a [`Ready`]'s), leaving files such as `last-result.json` alone.
/// Each failure is logged at warn; a missing `updates_root` has nothing to
/// delete.
pub fn clean_old(updates_root: &Path, keep: Option<&Path>) {
    let entries = match std::fs::read_dir(updates_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            warn!(root = %updates_root.display(), "update: could not list old downloads: {err}");
            return;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(err) => {
                warn!(root = %updates_root.display(), "update: could not read an old download: {err}");
                continue;
            }
        };
        if !path.is_dir() || keep.is_some_and(|keep| keep == path) {
            continue;
        }
        if let Err(err) = std::fs::remove_dir_all(&path) {
            warn!(folder = %path.display(), "update: could not delete an old download: {err}");
        }
    }
}

/// The named mutex an update holds while it starts or applies; the
/// installer's `apply-update` takes the same one.
pub const UPDATE_MUTEX: &str = r"Local\rustling-tulip-update";

/// The arguments the installer runs with for an update.
const INSTALLER_ARGS: [&str; 3] = ["/S", "/UPDATE", "/RELAUNCH"];

/// The outcome `apply-update` recorded in `updates/last-result.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LastResult {
    /// The version replaced; `None` on a first install.
    #[serde(default)]
    pub from: Option<String>,
    /// The version the update installs.
    pub to: String,
    /// Whether the update is in place.
    pub ok: bool,
    /// The line that says what happened.
    pub message: String,
}

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
    /// Checks the feed for a release newer than `current` and, when there is
    /// one, downloads and verifies it.
    fn check(
        &self,
        current: &semver::Version,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Option<Ready>>>;

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

    fn start_installer(&self, ready: &Ready) -> anyhow::Result<()> {
        let hold = lock::try_lock(UPDATE_MUTEX)?.ok_or(AlreadyRunning)?;
        let args: Vec<std::ffi::OsString> = INSTALLER_ARGS.iter().map(Into::into).collect();
        launch_detached(&ready.path, &args)?;
        info!(path = %ready.path.display(), "update: installer started");
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

/// Reads `path` as a [`LastResult`] and deletes it; a missing file is
/// `None`, and any other failure is logged at warn.
#[must_use]
pub fn take_last_result(path: &Path) -> Option<LastResult> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!(path = %path.display(), "update: could not read the last result: {err}");
            return None;
        }
    };
    if let Err(err) = std::fs::remove_file(path) {
        warn!(path = %path.display(), "update: could not delete the last result: {err}");
    }
    match serde_json::from_slice(&bytes) {
        Ok(result) => Some(result),
        Err(err) => {
            warn!(path = %path.display(), "update: the last result did not decode: {err}");
            None
        }
    }
}

/// Launch housekeeping: deletes the old downloads under `updates_root` and
/// the `*.old` files an update left beside the client exe. Best-effort; each
/// failure is logged at warn.
pub fn clean_at_launch(updates_root: anyhow::Result<PathBuf>) {
    match updates_root {
        Ok(root) => clean_old(&root, None),
        Err(err) => warn!("update: no updates folder to clean: {err:#}"),
    }
    let folder = std::env::current_exe().map(|exe| exe.parent().map(Path::to_path_buf));
    match folder {
        Ok(Some(folder)) => remove_old_files(&folder),
        Ok(None) => warn!("update: the client exe has no folder"),
        Err(err) => warn!("update: could not find the client exe: {err}"),
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

/// Starts `program` with no console and no std handles, breaking away from
/// this process's job when the job allows it, as `apply-update`'s relaunch
/// does.
fn launch_detached(program: &Path, args: &[std::ffi::OsString]) -> anyhow::Result<()> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        /// What `CreateProcess` returns when the caller's job forbids breakaway.
        const ERROR_ACCESS_DENIED: i32 = 5;
        command.creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
        match command.spawn() {
            Ok(_) => return Ok(()),
            Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                warn!("update: the job forbids breakaway; starting the installer inside it");
                command.creation_flags(DETACHED_PROCESS);
            }
            Err(err) => {
                return Err(err).with_context(|| format!("starting {}", program.display()));
            }
        }
    }
    command
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    Ok(())
}

/// The update lock: a named Windows mutex owned without waiting.
mod lock {
    /// Holds the lock until dropped.
    pub(super) type Hold = Box<dyn std::any::Any>;

    /// `Ok(None)` when another process holds the mutex `name`.
    #[cfg(windows)]
    pub(super) fn try_lock(name: &str) -> anyhow::Result<Option<Hold>> {
        use anyhow::Context as _;
        use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        use windows::core::PCWSTR;

        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated UTF-16 that outlives the call.
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }
            .with_context(|| format!("creating the mutex {name}"))?;
        let handle = MutexHandle(handle);
        // SAFETY: `handle.0` is the live mutex handle just created.
        let waited = unsafe { WaitForSingleObject(handle.0, 0) };
        if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
            Ok(Some(Box::new(OwnedMutex(handle))))
        } else if waited == WAIT_TIMEOUT {
            Ok(None)
        } else {
            anyhow::bail!("waiting on the mutex {name} returned {}", waited.0)
        }
    }

    /// No update lock where there is no installer.
    #[cfg(not(windows))]
    pub(super) fn try_lock(_name: &str) -> anyhow::Result<Option<Hold>> {
        Ok(Some(Box::new(())))
    }

    /// A mutex handle, closed on drop.
    #[cfg(windows)]
    struct MutexHandle(windows::Win32::Foundation::HANDLE);

    #[cfg(windows)]
    impl Drop for MutexHandle {
        fn drop(&mut self) {
            // SAFETY: the handle came from `CreateMutexW`, and this value owns
            // it alone and closes it once.
            if let Err(err) = unsafe { windows::Win32::Foundation::CloseHandle(self.0) } {
                tracing::warn!("update: closing the update mutex: {err}");
            }
        }
    }

    /// A mutex this thread owns, released on drop before its handle closes.
    #[cfg(windows)]
    struct OwnedMutex(MutexHandle);

    #[cfg(windows)]
    impl Drop for OwnedMutex {
        fn drop(&mut self) {
            // SAFETY: this thread owns the mutex behind the live handle.
            if let Err(err) = unsafe { windows::Win32::System::Threading::ReleaseMutex(self.0.0) } {
                tracing::warn!("update: releasing the update mutex: {err}");
            }
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        FEED_PATH, HASH_MISMATCH, Ready, check_and_download, parse_releases, parse_sha256,
    };
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// Only a hang guard: no test asserts how long anything took.
    const HANG_GUARD: Duration = Duration::from_secs(60);
    const BASE: &str = "https://example.test";
    const INSTALLER: &[u8] = b"stand-in installer bytes";
    const NAME: &str = "rustling-tulip-0.6.0-setup.exe";

    fn v(text: &str) -> semver::Version {
        semver::Version::parse(text).expect("a semver version")
    }

    /// A release tagged `tag` carrying the assets `names`, served under `base`.
    fn release(tag: &str, names: &[String], base: &str) -> Value {
        let assets: Vec<Value> = names
            .iter()
            .map(|name| json!({"name": name, "browser_download_url": format!("{base}/dl/{name}")}))
            .collect();
        json!({"tag_name": tag, "draft": false, "prerelease": true, "assets": assets})
    }

    fn installer_name(tag: &str) -> String {
        format!("rustling-tulip-{}-setup.exe", tag.trim_start_matches('v'))
    }

    /// A release tagged `tag` with both its assets.
    fn complete(tag: &str, base: &str) -> Value {
        let name = installer_name(tag);
        release(tag, &[format!("{name}.sha256"), name], base)
    }

    fn feed(releases: Vec<Value>) -> String {
        Value::Array(releases).to_string()
    }

    fn picked(releases: Vec<Value>, current: &str) -> Option<semver::Version> {
        parse_releases(&feed(releases), &v(current)).map(|offer| offer.version)
    }

    #[test]
    fn picks_the_newest_pre_release() {
        let json = feed(vec![
            complete("v0.5.1-alpha", BASE),
            complete("v0.6.0-beta.1", BASE),
            complete("v0.5.2-alpha", BASE),
        ]);
        let offer = parse_releases(&json, &v("0.5.0-alpha")).expect("an offer");
        let name = "rustling-tulip-0.6.0-beta.1-setup.exe";
        assert_eq!(offer.version, v("0.6.0-beta.1"));
        assert_eq!(offer.installer_name, name);
        assert_eq!(offer.installer_url, format!("{BASE}/dl/{name}"));
        assert_eq!(offer.sha256_url, format!("{BASE}/dl/{name}.sha256"));
    }

    #[test]
    fn skips_a_draft() {
        let mut draft = complete("v0.7.0", BASE);
        draft["draft"] = json!(true);
        assert_eq!(
            picked(vec![draft, complete("v0.6.0", BASE)], "0.5.0"),
            Some(v("0.6.0"))
        );
    }

    #[test]
    fn skips_a_release_missing_either_asset() {
        let no_hash = release("v0.8.0", &[installer_name("v0.8.0")], BASE);
        let no_installer = release(
            "v0.7.0",
            &[format!("{}.sha256", installer_name("v0.7.0"))],
            BASE,
        );
        assert_eq!(
            picked(
                vec![no_hash, no_installer, complete("v0.6.0", BASE)],
                "0.5.0"
            ),
            Some(v("0.6.0"))
        );
        let no_hash = release("v0.8.0", &[installer_name("v0.8.0")], BASE);
        assert_eq!(picked(vec![no_hash], "0.5.0"), None);
    }

    #[test]
    fn skips_one_not_newer() {
        assert_eq!(
            picked(
                vec![complete("v0.5.0", BASE), complete("v0.4.0", BASE)],
                "0.5.0"
            ),
            None
        );
        assert_eq!(
            picked(vec![complete("v0.5.0", BASE)], "0.5.0-alpha"),
            Some(v("0.5.0"))
        );
    }

    #[test]
    fn skips_an_unparsable_tag() {
        let releases = vec![
            complete("vnext", BASE),
            complete("0.9.0", BASE),
            complete("v0.8", BASE),
            complete("v0.6.0", BASE),
        ];
        assert_eq!(picked(releases, "0.5.0"), Some(v("0.6.0")));
    }

    #[test]
    fn picks_0_6_0_over_0_6_0_alpha() {
        let alpha_first = vec![complete("v0.6.0-alpha", BASE), complete("v0.6.0", BASE)];
        let alpha_last = vec![complete("v0.6.0", BASE), complete("v0.6.0-alpha", BASE)];
        assert_eq!(picked(alpha_first, "0.5.0"), Some(v("0.6.0")));
        assert_eq!(picked(alpha_last, "0.5.0"), Some(v("0.6.0")));
    }

    const HASH: &str = "ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01";

    #[test]
    fn a_sha256_line_naming_the_installer_parses() {
        let expected: [u8; 32] = std::array::from_fn(|i| if i % 2 == 0 { 0xab } else { 0x01 });
        let parsed = parse_sha256(&format!("{HASH}  {NAME}\n"), NAME).expect("a right line parses");
        assert_eq!(parsed, expected);
        assert_eq!(
            parse_sha256(&format!("{HASH}  {NAME}"), NAME).expect("no newline"),
            expected
        );
    }

    #[test]
    fn a_sha256_line_naming_another_file_is_refused() {
        let other = "rustling-tulip-0.5.0-setup.exe";
        assert!(parse_sha256(&format!("{HASH}  {other}\n"), NAME).is_err());
        assert!(
            parse_sha256(&format!("{HASH} {NAME}\n"), NAME).is_err(),
            "one space"
        );
    }

    #[test]
    fn a_short_sha256_is_refused() {
        assert!(parse_sha256(&format!("{}  {NAME}\n", &HASH[..62]), NAME).is_err());
        assert!(
            parse_sha256(&format!("{HASH}00  {NAME}\n"), NAME).is_err(),
            "too long"
        );
    }

    #[test]
    fn a_non_hex_sha256_is_refused() {
        assert!(parse_sha256(&format!("zz{}  {NAME}\n", &HASH[2..]), NAME).is_err());
    }

    #[test]
    fn a_crlf_sha256_line_is_tolerated() {
        assert!(parse_sha256(&format!("{HASH}  {NAME}\r\n"), NAME).is_ok());
    }

    /// A local HTTP server answering canned bodies by path, 404 otherwise, and
    /// reporting each request's path and lower-cased head.
    struct FeedServer {
        base: String,
        seen: mpsc::Receiver<(String, String)>,
    }

    impl FeedServer {
        fn requests(&self) -> Vec<(String, String)> {
            self.seen.try_iter().collect()
        }

        fn paths(requests: &[(String, String)]) -> Vec<&str> {
            requests.iter().map(|(path, _)| path.as_str()).collect()
        }
    }

    type Routes = Vec<(String, Vec<u8>)>;

    fn serve(routes: impl FnOnce(&str) -> Routes) -> FeedServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the feed server");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking listener");
        let base = format!("http://{}", listener.local_addr().expect("its address"));
        let routes = routes(&base);
        let (seen_tx, seen) = mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the feed server's runtime");
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("a tokio listener");
                while let Ok((stream, _)) = listener.accept().await {
                    answer(stream, &routes, &seen_tx).await;
                }
            });
        });
        FeedServer { base, seen }
    }

    async fn answer(
        mut stream: tokio::net::TcpStream,
        routes: &Routes,
        seen: &mpsc::Sender<(String, String)>,
    ) {
        let mut head = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => head.extend_from_slice(&buffer[..read]),
            }
        }
        let head = String::from_utf8_lossy(&head).to_lowercase();
        let path = head
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let body = routes
            .iter()
            .find(|(route, _)| route.to_lowercase() == path);
        let _ = seen.send((path, head.clone()));
        let (status, body) = match body {
            Some((_, body)) => ("200 OK", body.as_slice()),
            None => ("404 Not Found", b"not found".as_slice()),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.write_all(body).await;
        let _ = stream.shutdown().await;
    }

    fn hex_of(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        Sha256::digest(bytes)
            .iter()
            .fold(String::new(), |mut hex, b| {
                let _ = write!(hex, "{b:02x}");
                hex
            })
    }

    /// One release, `version`, serving `installer` and a `.sha256` of `hashed`.
    fn release_routes(
        version: &'static str,
        installer: &'static [u8],
        hashed: &'static [u8],
    ) -> impl FnOnce(&str) -> Routes {
        move |base| {
            let name = format!("rustling-tulip-{version}-setup.exe");
            vec![
                (
                    FEED_PATH.to_owned(),
                    feed(vec![complete(&format!("v{version}"), base)]).into_bytes(),
                ),
                (
                    format!("/dl/{name}.sha256"),
                    format!("{}  {name}\n", hex_of(hashed)).into_bytes(),
                ),
                (format!("/dl/{name}"), installer.to_vec()),
            ]
        }
    }

    /// A fresh updates root, not yet created.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rt-update-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn check(server: &FeedServer, current: &str, root: &Path) -> anyhow::Result<Option<Ready>> {
        let (done_tx, done) = mpsc::channel();
        let (base, current, root) = (server.base.clone(), v(current), root.to_path_buf());
        std::thread::spawn(move || {
            let _ = done_tx.send(check_and_download(&current, &root, &base));
        });
        done.recv_timeout(HANG_GUARD)
            .expect("hung: check_and_download never returned")
    }

    fn remove(root: &Path) {
        if let Err(err) = std::fs::remove_dir_all(root) {
            tracing::warn!("could not remove {}: {err}", root.display());
        }
    }

    #[test]
    fn a_newer_release_downloads_and_verifies() {
        let server = serve(release_routes("0.6.0", INSTALLER, INSTALLER));
        let root = scratch("verified");

        let ready = check(&server, "0.5.0", &root)
            .expect("the download verifies")
            .expect("an update is ready");

        assert_eq!(ready.version, v("0.6.0"));
        assert_eq!(
            std::fs::read(&ready.path).expect("read the installer"),
            INSTALLER
        );
        assert_eq!(ready.path.file_name().and_then(|n| n.to_str()), Some(NAME));
        let folder = ready.path.parent().expect("the download's folder");
        assert_eq!(folder.parent(), Some(root.as_path()));
        let folder_name = folder.file_name().and_then(|n| n.to_str()).expect("a name");
        let random = folder_name
            .strip_prefix("0.6.0-")
            .expect("<version>-<random>");
        assert!(
            random.len() == 8 && random.chars().all(|c| c.is_ascii_hexdigit()),
            "{folder_name}"
        );
        let requests = server.requests();
        assert_eq!(
            FeedServer::paths(&requests),
            [
                FEED_PATH.to_owned(),
                format!("/dl/{NAME}.sha256"),
                format!("/dl/{NAME}")
            ]
        );
        let feed_head = &requests[0].1;
        assert!(
            feed_head.contains("user-agent: rustling-tulip/0.5.0\r\n"),
            "{feed_head}"
        );
        assert!(
            feed_head.contains("accept: application/vnd.github+json\r\n"),
            "{feed_head}"
        );
        drop(ready);
        remove(&root);
    }

    #[test]
    fn a_hash_mismatch_deletes_the_download() {
        let server = serve(release_routes("0.6.0", INSTALLER, b"some other installer"));
        let root = scratch("mismatch");

        let err = check(&server, "0.5.0", &root).expect_err("a mismatched download is refused");

        assert_eq!(err.to_string(), HASH_MISMATCH);
        let left: Vec<_> = std::fs::read_dir(&root)
            .expect("the updates root exists")
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
        assert_eq!(
            FeedServer::paths(&server.requests()),
            [
                FEED_PATH.to_owned(),
                format!("/dl/{NAME}.sha256"),
                format!("/dl/{NAME}")
            ]
        );
        remove(&root);
    }

    #[test]
    fn no_newer_release_downloads_nothing() {
        let server = serve(release_routes("0.5.0", INSTALLER, INSTALLER));
        let root = scratch("none");

        let found = check(&server, "0.5.0", &root).expect("the check succeeds");

        assert!(found.is_none(), "offered {found:?}");
        assert_eq!(FeedServer::paths(&server.requests()), [FEED_PATH]);
        assert!(!root.exists(), "the updates root was created");
    }

    #[cfg(windows)]
    #[test]
    fn the_verified_file_cannot_be_replaced_while_ready_is_held() {
        let server = serve(release_routes("0.6.0", INSTALLER, INSTALLER));
        let root = scratch("held");
        let ready = check(&server, "0.5.0", &root)
            .expect("the download verifies")
            .expect("an update is ready");
        let decoy = root.join("decoy.exe");
        std::fs::write(&decoy, b"decoy").expect("write the decoy");

        assert!(
            std::fs::rename(&decoy, &ready.path).is_err(),
            "a rename over the verified file succeeded"
        );
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&ready.path)
                .is_err(),
            "the verified file opened for write"
        );
        assert!(
            std::fs::remove_file(&ready.path).is_err(),
            "the verified file was deleted"
        );
        assert_eq!(
            std::fs::read(&ready.path).expect("read the installer"),
            INSTALLER
        );

        let path = ready.path.clone();
        drop(ready);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("the file opens for write once Ready is dropped");
        remove(&root);
    }
}
