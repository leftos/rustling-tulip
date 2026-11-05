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

/// The feed's path below its base URL: 100 releases a page, so a changelog
/// covering a long gap between updates reaches every release.
const FEED_PATH: &str = "/repos/leftos/rustling-tulip/releases?per_page=100";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A bound on one whole request, the installer's download included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// A bound on the changelog's one feed request, which a reader waits on in
/// the What's new window.
const CHANGELOG_TIMEOUT: Duration = Duration::from_secs(30);

/// A release newer than the running client that the feed offers, with both
/// its assets' download URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// The release's version, from its `v<version>` tag.
    pub version: semver::Version,
    /// Where the update's asset for this platform downloads from.
    pub installer_url: String,
    /// Where the update asset's `.sha256` downloads from.
    pub sha256_url: String,
    /// The update's asset for this platform, as [`asset_name`] names it.
    pub installer_name: String,
    /// The changelog from the running version up to this one, as
    /// [`changelog_text`] joins it; empty until `pick` fills it from the
    /// feed's other releases.
    pub notes: String,
}

/// A downloaded installer whose SHA-256 matched its release. On Windows
/// `file` is held open with write and delete sharing denied, so the verified
/// file cannot be replaced for as long as this lives; on macOS the zip is only
/// read by `ditto`, and nothing holds it.
#[derive(Debug)]
pub struct Ready {
    /// The release's version.
    pub version: semver::Version,
    /// The installer, in its own `updates/<version>-<random>/` folder.
    pub path: PathBuf,
    /// The handle the hash was read through.
    pub file: File,
    /// The changelog from the version the update was offered over, as
    /// [`changelog_text`] joins it.
    pub notes: String,
}

/// One release in the feed; only the fields the pick and the changelog read.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    body: Option<String>,
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

/// A platform the release feed publishes update assets for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Windows: the NSIS setup exe.
    Windows,
    /// macOS on Apple silicon: a zip of the `.app`.
    MacArm64,
}

impl Platform {
    /// The platform this client runs on; `None` where the feed publishes no
    /// update asset for it.
    #[must_use]
    pub fn current() -> Option<Self> {
        #[cfg(windows)]
        {
            Some(Self::Windows)
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            Some(Self::MacArm64)
        }
        #[cfg(not(any(windows, all(target_os = "macos", target_arch = "aarch64"))))]
        {
            None
        }
    }
}

/// The update asset `raw_version` carries on `platform`: the setup exe on
/// Windows, the `.app` zip on Apple-silicon macOS. Its `.sha256` asset is
/// this name with `.sha256` appended.
#[must_use]
pub fn asset_name(raw_version: &str, platform: Platform) -> String {
    let suffix = match platform {
        Platform::Windows => "setup.exe",
        Platform::MacArm64 => "macos-arm64.zip",
    };
    format!("rustling-tulip-{raw_version}-{suffix}")
}

/// The newest non-draft release in the feed `json` strictly newer than
/// `current` by semver, pre-releases included, that carries both this
/// platform's update asset and its `.sha256`. A release whose tag is not
/// `v<version>` is skipped; a feed that does not decode is logged and offers
/// nothing, and so does a platform the feed publishes no asset for.
#[must_use]
pub fn parse_releases(json: &str, current: &semver::Version) -> Option<Offer> {
    let platform = this_platform()?;
    parse_releases_for(json, current, platform)
}

/// [`parse_releases`] for `platform`: the newest release carrying both that
/// platform's update asset and the asset's `.sha256`.
#[must_use]
pub fn parse_releases_for(
    json: &str,
    current: &semver::Version,
    platform: Platform,
) -> Option<Offer> {
    match decode_releases(json) {
        Ok(releases) => pick(&releases, current, platform),
        Err(err) => {
            warn!("update: {err:#}");
            None
        }
    }
}

/// [`Platform::current`], logging when this platform has no update asset.
fn this_platform() -> Option<Platform> {
    let platform = Platform::current();
    if platform.is_none() {
        tracing::debug!("update: no release asset for this platform");
    }
    platform
}

fn decode_releases(json: &str) -> anyhow::Result<Vec<Release>> {
    serde_json::from_str(json).context("the release feed did not decode")
}

/// See [`parse_releases`].
fn pick(releases: &[Release], current: &semver::Version, platform: Platform) -> Option<Offer> {
    let mut offer = releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| offer_of(release, platform))
        .filter(|offer| offer.version > *current)
        .max_by(|a, b| a.version.cmp(&b.version))?;
    offer.notes = changelog_text(&notes_between(releases, Some(current), &offer.version));
    Some(offer)
}

/// The offer `release` makes on `platform`: its tag `v<version>` parsed, and
/// both assets found by name.
fn offer_of(release: &Release, platform: Platform) -> Option<Offer> {
    let raw = release.tag_name.strip_prefix('v')?;
    let version = semver::Version::parse(raw).ok()?;
    let installer_name = asset_name(raw, platform);
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
        notes: String::new(),
    })
}

/// One release's notes, from its feed body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseNotes {
    /// The release's version, from its `v<version>` tag.
    pub version: semver::Version,
    /// The release's notes, with `\r\n` turned into `\n` and trimmed.
    pub body: String,
}

impl ReleaseNotes {
    /// The notes `release` carries: its tag parsed as `v<version>` and its
    /// body normalized; `None` when the tag is not `v<version>` or the body
    /// is empty.
    fn of(release: &Release) -> Option<Self> {
        let version = semver::Version::parse(release.tag_name.strip_prefix('v')?).ok()?;
        let body = release.body.as_deref()?.replace("\r\n", "\n");
        let body = body.trim().to_owned();
        if body.is_empty() {
            return None;
        }
        Some(Self { version, body })
    }
}

/// The notes of every release in `releases` in range, newest first: newer
/// than `after` and no newer than `through`, or `through` alone when `after`
/// is unknown. A draft, a tag that is not `v<version>` and a release without
/// notes are left out.
fn notes_between(
    releases: &[Release],
    after: Option<&semver::Version>,
    through: &semver::Version,
) -> Vec<ReleaseNotes> {
    let in_range = |version: &semver::Version| match after {
        Some(after) => version > after && version <= through,
        None => version == through,
    };
    let mut notes: Vec<ReleaseNotes> = releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(ReleaseNotes::of)
        .filter(|notes| in_range(&notes.version))
        .collect();
    notes.sort_by_key(|notes| std::cmp::Reverse(notes.version.clone()));
    notes
}

/// The changelog `notes` join to, newest first: for each release its
/// `## v<version>` heading, a blank line and its body, releases separated by
/// one blank line, with no trailing newline. No notes is the empty string.
#[must_use]
pub fn changelog_text(notes: &[ReleaseNotes]) -> String {
    notes
        .iter()
        .map(|notes| format!("## v{}\n\n{}", notes.version, notes.body))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The changelog from the feed `json`: every release after `after` and up to
/// `through`, as [`changelog_text`] joins them. The pure entry the tests use;
/// [`parse_releases`] logs a feed that does not decode, this returns it.
///
/// # Errors
///
/// Fails when the feed does not decode.
pub fn parse_changelog(
    json: &str,
    after: Option<&semver::Version>,
    through: &semver::Version,
) -> anyhow::Result<String> {
    let releases = decode_releases(json)?;
    Ok(changelog_text(&notes_between(&releases, after, through)))
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
    http_client_within(current, REQUEST_TIMEOUT)
}

/// The update HTTP client, each whole request bounded by `timeout`.
fn http_client_within(
    current: &semver::Version,
    timeout: Duration,
) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("rustling-tulip/{current}"))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()
        .context("building the update HTTP client")
}

async fn fetch_offer(
    http: &reqwest::Client,
    current: &semver::Version,
    feed_base: &str,
) -> anyhow::Result<Option<Offer>> {
    let Some(platform) = this_platform() else {
        return Ok(None);
    };
    let body = fetch_feed(http, feed_base).await?;
    Ok(pick(&decode_releases(&body)?, current, platform))
}

/// The feed's body from `feed_base`.
async fn fetch_feed(http: &reqwest::Client, feed_base: &str) -> anyhow::Result<String> {
    let url = format!("{}{FEED_PATH}", feed_base.trim_end_matches('/'));
    http.get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("fetching the release feed {url}"))?
        .text()
        .await
        .with_context(|| format!("reading the release feed {url}"))
}

/// Fetches the feed at `feed_base` and returns the changelog for the releases
/// after `after` and up to `through`, as [`changelog_text`] joins them. The
/// request runs on a thread of its own with a current-thread runtime; this
/// blocks until it finishes, so call it from a background task.
///
/// # Errors
///
/// Fails when the feed cannot be fetched or decoded.
pub fn fetch_changelog(
    after: Option<&semver::Version>,
    through: &semver::Version,
    feed_base: &str,
) -> anyhow::Result<String> {
    let (after, through, feed_base) = (after.cloned(), through.clone(), feed_base.to_owned());
    let worker = std::thread::Builder::new()
        .name("update-changelog".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("building the update thread's runtime")?;
            runtime.block_on(async {
                let http = http_client_within(&through, CHANGELOG_TIMEOUT)?;
                let body = fetch_feed(&http, &feed_base).await?;
                parse_changelog(&body, after.as_ref(), &through)
            })
        })
        .context("spawning the update thread")?;
    worker
        .join()
        .map_err(|_| anyhow!("the update thread panicked"))?
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
        notes: offer.notes.clone(),
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

/// The prefix of the hidden folder a macOS update unpacks the downloaded app
/// into, beside the installed bundle: the next launch removes it.
const STAGING_PREFIX: &str = ".rustling-tulip-update-";

/// The arguments the installer runs with for an update.
#[cfg(windows)]
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
        let hold = lock::try_lock(UPDATE_MUTEX)?.ok_or(AlreadyRunning)?;
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
    launch_detached(&ready.path, &args)?;
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
    launch_detached(
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

/// Starts `program` with no console and no std handles, breaking away from
/// this process's job when the job allows it, as `apply-update`'s relaunch
/// does.
#[cfg(any(windows, target_os = "macos"))]
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Its own process group, so an interrupt or a hang-up in the terminal
        // that started the client does not reach the update it started.
        command.process_group(0);
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

    /// No cross-process update lock off Windows: one client quits as soon as
    /// it starts an update, so a single client never presses twice, and two
    /// clients updating at once are not excluded.
    #[cfg(not(windows))]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the Windows mutex can fail; off Windows there is no lock to take"
    )]
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
    #[cfg(target_os = "macos")]
    use super::{
        BUNDLE_NAME, CLIENT_VERSION, UPDATE_PROBE, UPDATE_REFUSED, run_bounded, start_macos_update,
    };
    use super::{
        FEED_PATH, HASH_MISMATCH, Platform, Ready, STAGING_PREFIX, apply_update_args, asset_name,
        bundle_of, check_and_download, fetch_changelog, parse_changelog, parse_releases_for,
        parse_sha256, remove_bundle_leftovers,
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

    /// The update asset this platform's feed carries for `tag`.
    fn current_asset(tag: &str) -> String {
        asset_name(
            tag.trim_start_matches('v'),
            Platform::current().unwrap_or(Platform::Windows),
        )
    }

    /// The asset name the download tests serve and expect.
    fn name() -> String {
        current_asset("v0.6.0")
    }

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

    /// `release` with its notes set to `body`.
    fn with_body(mut release: Value, body: &str) -> Value {
        release["body"] = json!(body);
        release
    }

    fn feed(releases: Vec<Value>) -> String {
        Value::Array(releases).to_string()
    }

    fn picked(releases: Vec<Value>, current: &str) -> Option<semver::Version> {
        parse_releases_for(&feed(releases), &v(current), Platform::Windows)
            .map(|offer| offer.version)
    }

    #[test]
    fn picks_the_newest_pre_release() {
        let json = feed(vec![
            complete("v0.5.1-alpha", BASE),
            complete("v0.6.0-beta.1", BASE),
            complete("v0.5.2-alpha", BASE),
        ]);
        let offer =
            parse_releases_for(&json, &v("0.5.0-alpha"), Platform::Windows).expect("an offer");
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

    #[test]
    fn asset_name_follows_the_platform() {
        assert_eq!(
            asset_name("0.9.0-alpha", Platform::Windows),
            "rustling-tulip-0.9.0-alpha-setup.exe"
        );
        assert_eq!(
            asset_name("0.9.0-alpha", Platform::MacArm64),
            "rustling-tulip-0.9.0-alpha-macos-arm64.zip"
        );
    }

    /// A release tagged `tag` carrying both the Windows pair and the mac zip
    /// pair.
    fn complete_both(tag: &str, base: &str) -> Value {
        let raw = tag.trim_start_matches('v');
        let windows = asset_name(raw, Platform::Windows);
        let mac = asset_name(raw, Platform::MacArm64);
        release(
            tag,
            &[
                format!("{windows}.sha256"),
                windows,
                format!("{mac}.sha256"),
                mac,
            ],
            base,
        )
    }

    /// A release tagged `tag` carrying only the mac zip pair.
    fn complete_mac(tag: &str, base: &str) -> Value {
        let name = asset_name(tag.trim_start_matches('v'), Platform::MacArm64);
        release(tag, &[format!("{name}.sha256"), name], base)
    }

    #[test]
    fn a_mac_client_is_offered_the_mac_zip() {
        let json = feed(vec![complete_both("v0.9.0-alpha", BASE)]);

        let offer = parse_releases_for(&json, &v("0.8.0"), Platform::MacArm64).expect("an offer");

        let name = "rustling-tulip-0.9.0-alpha-macos-arm64.zip";
        assert_eq!(offer.version, v("0.9.0-alpha"));
        assert_eq!(offer.installer_name, name);
        assert_eq!(offer.installer_url, format!("{BASE}/dl/{name}"));
        assert_eq!(offer.sha256_url, format!("{BASE}/dl/{name}.sha256"));
    }

    #[test]
    fn a_windows_client_ignores_the_mac_assets() {
        let json = feed(vec![complete_both("v0.9.0-alpha", BASE)]);

        let offer = parse_releases_for(&json, &v("0.8.0"), Platform::Windows).expect("an offer");

        let name = "rustling-tulip-0.9.0-alpha-setup.exe";
        assert_eq!(offer.version, v("0.9.0-alpha"));
        assert_eq!(offer.installer_name, name);
        assert_eq!(offer.installer_url, format!("{BASE}/dl/{name}"));
        assert_eq!(offer.sha256_url, format!("{BASE}/dl/{name}.sha256"));
    }

    #[test]
    fn a_release_missing_this_platforms_pair_is_skipped() {
        let newer_windows_only = feed(vec![
            complete("v0.9.0", BASE),
            complete_both("v0.8.0", BASE),
        ]);

        assert_eq!(
            parse_releases_for(&newer_windows_only, &v("0.5.0"), Platform::MacArm64)
                .map(|offer| offer.version),
            Some(v("0.8.0"))
        );

        let mac_only = feed(vec![complete_mac("v0.9.0", BASE)]);
        assert_eq!(
            parse_releases_for(&mac_only, &v("0.5.0"), Platform::Windows),
            None
        );
    }

    const HASH: &str = "ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01";

    #[test]
    fn a_sha256_line_naming_the_installer_parses() {
        let expected: [u8; 32] = std::array::from_fn(|i| if i % 2 == 0 { 0xab } else { 0x01 });
        let name = name();
        let parsed =
            parse_sha256(&format!("{HASH}  {name}\n"), &name).expect("a right line parses");
        assert_eq!(parsed, expected);
        assert_eq!(
            parse_sha256(&format!("{HASH}  {name}"), &name).expect("no newline"),
            expected
        );
    }

    #[test]
    fn a_sha256_line_naming_another_file_is_refused() {
        let other = "rustling-tulip-0.5.0-setup.exe";
        let name = name();
        assert!(parse_sha256(&format!("{HASH}  {other}\n"), &name).is_err());
        assert!(
            parse_sha256(&format!("{HASH} {name}\n"), &name).is_err(),
            "one space"
        );
    }

    #[test]
    fn a_short_sha256_is_refused() {
        let name = name();
        assert!(parse_sha256(&format!("{}  {name}\n", &HASH[..62]), &name).is_err());
        assert!(
            parse_sha256(&format!("{HASH}00  {name}\n"), &name).is_err(),
            "too long"
        );
    }

    #[test]
    fn a_non_hex_sha256_is_refused() {
        let name = name();
        assert!(parse_sha256(&format!("zz{}  {name}\n", &HASH[2..]), &name).is_err());
    }

    #[test]
    fn a_crlf_sha256_line_is_tolerated() {
        let name = name();
        assert!(parse_sha256(&format!("{HASH}  {name}\r\n"), &name).is_ok());
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

    /// The notes `release_routes` serves with its release.
    const RELEASE_NOTES: &str = "### Added\n\n- an update's notes";

    /// One release, `version`, serving `installer` and a `.sha256` of `hashed`.
    fn release_routes(
        version: &'static str,
        installer: &'static [u8],
        hashed: &'static [u8],
    ) -> impl FnOnce(&str) -> Routes {
        move |base| {
            let tag = format!("v{version}");
            let name = current_asset(&tag);
            vec![
                (
                    FEED_PATH.to_owned(),
                    feed(vec![with_body(
                        release(&tag, &[format!("{name}.sha256"), name.clone()], base),
                        RELEASE_NOTES,
                    )])
                    .into_bytes(),
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
        assert_eq!(
            ready.path.file_name().and_then(|n| n.to_str()),
            Some(name().as_str())
        );
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
                format!("/dl/{}.sha256", name()),
                format!("/dl/{}", name())
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
                format!("/dl/{}.sha256", name()),
                format!("/dl/{}", name())
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

    /// [`fetch_changelog`] against `server`, off this thread and bounded by
    /// [`HANG_GUARD`].
    fn fetched_changelog(
        server: &FeedServer,
        after: Option<&str>,
        through: &str,
    ) -> anyhow::Result<String> {
        let (done_tx, done) = mpsc::channel();
        let (base, after, through) = (server.base.clone(), after.map(v), v(through));
        std::thread::spawn(move || {
            let _ = done_tx.send(fetch_changelog(after.as_ref(), &through, &base));
        });
        done.recv_timeout(HANG_GUARD)
            .expect("hung: fetch_changelog never returned")
    }

    #[test]
    fn changelog_covers_every_release_after_the_last_version_newest_first() {
        let json = feed(vec![
            with_body(complete("v0.5.0-alpha", BASE), "five"),
            with_body(complete("v0.6.0-alpha", BASE), "six"),
            with_body(complete("v0.7.0-alpha", BASE), "seven"),
            with_body(complete("v0.8.0-alpha", BASE), "eight"),
        ]);

        let text = parse_changelog(&json, Some(&v("0.5.0-alpha")), &v("0.7.0-alpha"))
            .expect("the feed decodes");

        assert_eq!(text, "## v0.7.0-alpha\n\nseven\n\n## v0.6.0-alpha\n\nsix");
    }

    #[test]
    fn changelog_with_no_last_version_is_the_new_release_alone() {
        let json = feed(vec![
            with_body(complete("v0.6.0-alpha", BASE), "six"),
            with_body(complete("v0.7.0-alpha", BASE), "seven"),
        ]);

        let text = parse_changelog(&json, None, &v("0.7.0-alpha")).expect("the feed decodes");

        assert_eq!(text, "## v0.7.0-alpha\n\nseven");
    }

    #[test]
    fn changelog_includes_a_release_without_installer_assets() {
        let json = feed(vec![with_body(release("v0.6.0", &[], BASE), "six")]);

        let text = parse_changelog(&json, None, &v("0.6.0")).expect("the feed decodes");

        assert_eq!(text, "## v0.6.0\n\nsix");
    }

    #[test]
    fn changelog_leaves_out_drafts_bad_tags_and_empty_notes() {
        let mut draft = with_body(complete("v0.6.0", BASE), "a draft");
        draft["draft"] = json!(true);
        let mut null_body = complete("v0.7.0", BASE);
        null_body["body"] = Value::Null;
        let json = feed(vec![
            with_body(complete("v0.5.0", BASE), "kept"),
            draft,
            with_body(release("nightly", &[], BASE), "a nightly"),
            null_body,
            with_body(complete("v0.8.0", BASE), "  \n"),
        ]);

        let text =
            parse_changelog(&json, Some(&v("0.4.0")), &v("0.8.0")).expect("the feed decodes");

        assert_eq!(text, "## v0.5.0\n\nkept");
    }

    #[test]
    fn changelog_turns_crlf_into_lf_and_trims() {
        let json = feed(vec![with_body(
            complete("v0.6.0", BASE),
            "\r\n### Fixed\r\n\r\n- a fix\r\n",
        )]);

        let text = parse_changelog(&json, None, &v("0.6.0")).expect("the feed decodes");

        assert_eq!(text, "## v0.6.0\n\n### Fixed\n\n- a fix");
    }

    #[test]
    fn an_empty_range_is_an_empty_changelog() {
        let json = feed(vec![with_body(complete("v0.6.0", BASE), "six")]);

        let text =
            parse_changelog(&json, Some(&v("0.6.0")), &v("0.6.0")).expect("the feed decodes");

        assert!(text.is_empty(), "{text:?}");
    }

    #[test]
    fn a_feed_that_does_not_decode_is_a_changelog_error() {
        assert!(parse_changelog("not json", None, &v("0.6.0")).is_err());
    }

    #[test]
    fn an_offer_carries_the_notes_since_the_running_version() {
        let json = feed(vec![
            with_body(complete("v0.5.0", BASE), "five"),
            with_body(complete("v0.6.0", BASE), "six"),
            with_body(complete("v0.7.0", BASE), "seven"),
        ]);

        let offer = parse_releases_for(&json, &v("0.5.0"), Platform::Windows).expect("an offer");

        assert_eq!(offer.version, v("0.7.0"));
        assert_eq!(offer.notes, "## v0.7.0\n\nseven\n\n## v0.6.0\n\nsix");
    }

    #[test]
    fn a_downloaded_update_carries_its_notes() {
        let server = serve(release_routes("0.6.0", INSTALLER, INSTALLER));
        let root = scratch("notes");

        let ready = check(&server, "0.5.0", &root)
            .expect("the download verifies")
            .expect("an update is ready");

        assert_eq!(ready.notes, format!("## v0.6.0\n\n{RELEASE_NOTES}"));
        remove(&root);
    }

    #[test]
    fn fetch_changelog_reads_the_range_from_the_feed() {
        let server = serve(|base| {
            vec![(
                FEED_PATH.to_owned(),
                feed(vec![
                    with_body(complete("v0.5.0", base), "five"),
                    with_body(complete("v0.6.0", base), "six"),
                    with_body(complete("v0.7.0", base), "seven"),
                ])
                .into_bytes(),
            )]
        });

        let text = fetched_changelog(&server, Some("0.5.0"), "0.7.0").expect("the feed decodes");

        assert_eq!(text, "## v0.7.0\n\nseven\n\n## v0.6.0\n\nsix");
        assert_eq!(FeedServer::paths(&server.requests()), [FEED_PATH]);
    }

    #[test]
    fn fetch_changelog_fails_when_the_feed_is_missing() {
        let server = serve(|_base| Vec::new());

        assert!(fetched_changelog(&server, None, "0.7.0").is_err());
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
