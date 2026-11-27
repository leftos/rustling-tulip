//! The phone's share inbox: the folder other apps' shares land in, read on
//! the phone's side.
//!
//! The share extension writes each shared item as a payload, then its
//! manifest beside it by an atomic rename: `{id}.bin` holds the bytes and
//! `{id}.json` says what they are ([`Manifest`]). The app reads only
//! `*.json` files, so an item whose manifest has not landed is not seen; a
//! file item whose payload is missing waits, and is stale once a day old.
//!
//! The extension is another process, so nothing a manifest says is trusted
//! as a path: every file name in it is one plain name inside the inbox
//! folder, and only regular files there are read.
//!
//! [`LastUsed`] is the phone's record of when each session was last opened
//! or sent a share, so the share sheet can offer the last-used session
//! first.

use std::collections::BTreeMap;
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::target::HostKey;

/// The variable that names the inbox folder, absolute; a relative or empty
/// value counts as unset.
pub const INBOX_ENV: &str = "RUSTLING_TULIP_SHARE_INBOX_DIR";

/// The App Group the phone app and its share extension share.
pub const GROUP_ID: &str = "group.dev.leftos.rustling-tulip.phone";

/// The inbox folder inside the App Group container.
pub const GROUP_FOLDER: &str = "ShareInbox";

/// The inbox folder inside the phone's UI folder, when there is no group.
pub const UI_FOLDER: &str = "share-inbox";

/// The last-used record's file, in the UI folder beside `fetched.json`.
pub const LAST_USED_FILE: &str = "recent-sessions.json";

/// How long a file item may wait for its payload before it is stale: a day,
/// in milliseconds.
pub const STALE_MS: i64 = 24 * 60 * 60 * 1000;

/// The most a manifest may hold: 1 MiB; a larger one is not valid.
pub const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;

/// The manifest version this build reads.
const MANIFEST_VERSION: u32 = 1;

/// How many sessions the last-used record keeps: the most recently used.
pub const LAST_USED_MAX: usize = 50;

/// The name extensions of a shared file that goes up as a pasted image,
/// matched in any case.
const IMAGE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "gif", "heic", "webp"];

/// What a shared item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A file: its bytes are the payload, its name the one it had.
    File,
    /// Plain text, held in the manifest's `text`.
    Text,
    /// A link, held in the manifest's `text`.
    Url,
}

/// One shared item's manifest, `{id}.json` in the inbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The manifest version; only [`MANIFEST_VERSION`] is read.
    pub v: u32,
    /// The item's id, which names its files.
    pub id: String,
    /// What the item is.
    pub kind: Kind,
    /// The file's name as it was shared; required for a file.
    #[serde(default)]
    pub name: Option<String>,
    /// The payload's file name inside the inbox; required for a file.
    #[serde(default)]
    pub payload: Option<String>,
    /// The text or the link; required for text and a link.
    #[serde(default)]
    pub text: Option<String>,
    /// When the item was shared, in milliseconds since the Unix epoch.
    pub created: i64,
}

impl Manifest {
    /// The manifest `bytes` hold, checked: the version is 1, every file
    /// name is one plain name, and the fields its kind needs are there.
    ///
    /// # Errors
    ///
    /// Fails with why when the bytes are not a manifest or it is not valid.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let manifest: Self = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Checks the manifest as [`Self::parse`] describes.
    fn validate(&self) -> Result<(), String> {
        if self.v != MANIFEST_VERSION {
            return Err(format!(
                "manifest version {} is not {MANIFEST_VERSION}",
                self.v
            ));
        }
        plain_name("id", Some(&self.id))?;
        plain_name("name", self.name.as_deref())?;
        match self.kind {
            Kind::File if self.name.is_none() => Err("a file item has no name".to_owned()),
            Kind::File if self.payload.as_deref() != Some(self.payload_name().as_str()) => Err(
                format!("a file item's payload is not {}", self.payload_name()),
            ),
            Kind::Text | Kind::Url if self.payload.is_some() => {
                Err("a text or link item names a payload".to_owned())
            }
            Kind::Text | Kind::Url if self.text.is_none() => {
                Err("a text or link item has no text".to_owned())
            }
            _ => Ok(()),
        }
    }

    /// The manifest's own file name, `{id}.json`.
    fn manifest_name(&self) -> String {
        format!("{}.json", self.id)
    }

    /// A file item's payload file name, `{id}.bin`.
    fn payload_name(&self) -> String {
        format!("{}.bin", self.id)
    }
}

/// Refuses `value`, the manifest's `field`, unless it is one plain file
/// name: not empty, not `.` or `..`, and with no path separator or NUL.
/// An absent value passes.
fn plain_name(field: &str, value: Option<&str>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let mut parts = Path::new(value).components();
    let one_part = matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(part)), None) if part == value
    );
    let plain = one_part && !value.contains(['/', '\\', ':', '\0']);
    if plain {
        Ok(())
    } else {
        Err(format!("the {field} {value:?} is not a plain file name"))
    }
}

/// One item ready to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// What the item is.
    pub manifest: Manifest,
    /// Its manifest's file.
    pub json_path: PathBuf,
    /// Its payload's file, when the manifest names one.
    pub payload_path: Option<PathBuf>,
}

/// What one read of the inbox found.
#[derive(Debug, Default)]
pub struct Scan {
    /// The items ready to send, oldest first, then by id.
    pub ready: Vec<Item>,
    /// The manifests of file items whose payload is still missing a day
    /// after they were shared.
    pub stale: Vec<PathBuf>,
    /// The manifests that could not be read or are not valid.
    pub bad: Vec<PathBuf>,
    /// The payloads `{id}.bin` with no `{id}.json` beside them, last
    /// modified a day or more ago.
    pub orphans: Vec<PathBuf>,
}

/// What one manifest file held.
enum Found {
    /// An item ready to send.
    Ready(Item),
    /// A file item whose payload has not landed, shared at this time.
    Waiting(i64),
    /// Not a valid manifest, for this reason.
    Bad(String),
}

/// Reads the inbox at `dir` at `now`: each regular `*.json` file in it is
/// a manifest, every other file (a `.tmp` one included) is passed over. A
/// missing folder is an empty inbox.
#[must_use]
pub fn scan(dir: &Path, now: SystemTime) -> Scan {
    let mut scan = Scan::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            if err.kind() != io::ErrorKind::NotFound {
                tracing::warn!(dir = %dir.display(), error = %err, "phone: reading the share inbox");
            }
            return scan;
        }
    };
    let now_ms = millis(now);
    let paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    for path in paths.iter().cloned() {
        if !is_manifest_file(&path) {
            continue;
        }
        match read_item(dir, &path) {
            Found::Ready(item) => scan.ready.push(item),
            Found::Waiting(created) if now_ms.saturating_sub(created) >= STALE_MS => {
                scan.stale.push(path);
            }
            Found::Waiting(_) => {}
            Found::Bad(why) => {
                tracing::warn!(path = %path.display(), why, "phone: a share inbox manifest is not valid");
                scan.bad.push(path);
            }
        }
    }
    scan.ready.sort_by(|a, b| {
        (a.manifest.created, &a.manifest.id).cmp(&(b.manifest.created, &b.manifest.id))
    });
    scan.orphans = paths
        .into_iter()
        .filter(|path| is_orphan(path, now))
        .collect();
    scan
}

/// Whether `path` is a regular `*.bin` file with no manifest of its name
/// beside it, last modified at least [`STALE_MS`] before `now`.
fn is_orphan(path: &Path, now: SystemTime) -> bool {
    if path.extension().is_none_or(|ext| ext != "bin")
        || std::fs::symlink_metadata(path.with_extension("json")).is_ok()
    {
        return false;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let age = meta
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok());
    meta.is_file() && age.is_some_and(|age| age >= Duration::from_millis(STALE_MS.unsigned_abs()))
}

/// Whether a shared file named `name` goes up as a pasted image: its
/// extension is one of [`IMAGE_EXTENSIONS`], in any case.
#[must_use]
pub fn is_image(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

/// What a composer holding `existing` gets at its end for the text and
/// link items among `items`: each on a line of its own, after a line break
/// when `existing` is not empty and does not end in one; empty when no item
/// is text or a link.
#[must_use]
pub fn text_addition(existing: &str, items: &[Item]) -> String {
    let lines: Vec<&str> = items
        .iter()
        .filter(|item| item.manifest.kind != Kind::File)
        .filter_map(|item| item.manifest.text.as_deref())
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let joined = lines.join("\n");
    if existing.is_empty() || existing.ends_with('\n') {
        joined
    } else {
        format!("\n{joined}")
    }
}

/// A session the share sheet offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareRow {
    /// The session's host.
    pub host: HostKey,
    /// The session's id.
    pub session_id: String,
    /// The session's title, as Home lists it.
    pub label: String,
    /// The host's name, drawn under the title, for a session on a remote
    /// host.
    pub host_name: Option<String>,
}

impl ShareRow {
    /// The row's session's [`LastUsed::key`].
    #[must_use]
    pub fn key(&self) -> String {
        LastUsed::key(self.host.wire(), &self.session_id)
    }
}

/// The open share sheet: the items waiting in the inbox, which all go to
/// the one session picked, and the sessions it offers, most recently used
/// first.
#[derive(Clone, Debug)]
pub struct ShareSheet {
    /// The items ready to send, as the scan found them.
    pub items: Vec<Item>,
    /// The sessions offered, in the order the sheet draws them.
    pub rows: Vec<ShareRow>,
}

impl ShareSheet {
    /// The sheet sending `items` to one of `rows`, ordered by
    /// [`LastUsed::order`] on `last_used`.
    #[must_use]
    pub fn new(items: Vec<Item>, rows: Vec<ShareRow>, last_used: &LastUsed) -> Self {
        let keys: Vec<String> = rows.iter().map(ShareRow::key).collect();
        let mut rows: Vec<Option<ShareRow>> = rows.into_iter().map(Some).collect();
        let rows = last_used
            .order(&keys)
            .iter()
            .filter_map(|key| {
                let at = keys.iter().position(|known| known == key)?;
                rows.get_mut(at)?.take()
            })
            .collect();
        Self { items, rows }
    }

    /// The item count under the title: "1 item" or "N items".
    #[must_use]
    pub fn count_label(&self) -> String {
        match self.items.len() {
            1 => "1 item".to_owned(),
            count => format!("{count} items"),
        }
    }

    /// The row at `index` and the items it is sent; the sheet back when it
    /// has no such row.
    ///
    /// # Errors
    ///
    /// Gives the sheet back unchanged when `index` names no row.
    pub fn pick(mut self, index: usize) -> Result<(ShareRow, Vec<Item>), Self> {
        if index >= self.rows.len() {
            return Err(self);
        }
        let row = self.rows.swap_remove(index);
        Ok((row, self.items))
    }
}

/// Whether `path` is a regular file named `*.json`, not reached through a
/// link.
fn is_manifest_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "json")
        && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// The item the manifest at `path`, in the inbox `dir`, describes.
fn read_item(dir: &Path, path: &Path) -> Found {
    let manifest = match read_manifest(path).and_then(|bytes| Manifest::parse(&bytes)) {
        Ok(manifest) => manifest,
        Err(why) => return Found::Bad(why),
    };
    if path.file_name() != Some(manifest.manifest_name().as_ref()) {
        return Found::Bad(format!(
            "the manifest is not named {}",
            manifest.manifest_name()
        ));
    }
    let payload_path = (manifest.kind == Kind::File).then(|| dir.join(manifest.payload_name()));
    if manifest.kind == Kind::File {
        let meta = payload_path
            .as_deref()
            .and_then(|payload| std::fs::symlink_metadata(payload).ok());
        match meta {
            None => return Found::Waiting(manifest.created),
            Some(meta) if !meta.is_file() => {
                return Found::Bad("the payload is not a regular file".to_owned());
            }
            Some(_) => {}
        }
    }
    Found::Ready(Item {
        manifest,
        json_path: path.to_path_buf(),
        payload_path,
    })
}

/// The bytes of the manifest at `path`, opened once: the open file must be
/// a regular file, and at most [`MANIFEST_MAX_BYTES`] are read.
fn read_manifest(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    let meta = file.metadata().map_err(|err| err.to_string())?;
    if !meta.is_file() {
        return Err("the manifest is not a regular file".to_owned());
    }
    let mut bytes = Vec::new();
    file.take(MANIFEST_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > MANIFEST_MAX_BYTES) {
        return Err(format!("the manifest is over {MANIFEST_MAX_BYTES} bytes"));
    }
    Ok(bytes)
}

/// Removes `item` from the inbox: its payload first, its manifest last, so
/// a failure part way leaves the item to be read again rather than a
/// payload nothing names. A file already gone is no error.
///
/// # Errors
///
/// Fails with the first delete that failed for another reason; a failed
/// payload delete leaves the manifest.
pub fn remove(item: &Item) -> io::Result<()> {
    if let Some(payload) = &item.payload_path {
        remove_if_present(payload)?;
    }
    remove_if_present(&item.json_path)
}

/// The inbox folder: `env`, the value of [`INBOX_ENV`], when it is an
/// absolute path; else `group`, the App Group's inbox when the group
/// resolves; else [`UI_FOLDER`] in `ui_dir`. A relative or empty `env` is
/// logged and passed over.
#[must_use]
pub fn inbox_dir(env: Option<&str>, group: Option<PathBuf>, ui_dir: &Path) -> PathBuf {
    if let Some(value) = env {
        let path = Path::new(value);
        if !value.is_empty() && path.is_absolute() {
            return path.to_path_buf();
        }
        tracing::warn!(
            value,
            "phone: {INBOX_ENV} is not an absolute path, so it is ignored"
        );
    }
    group.unwrap_or_else(|| ui_dir.join(UI_FOLDER))
}

/// When each session was last opened or sent a share, in seconds since the
/// Unix epoch, as [`LAST_USED_FILE`] holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastUsed {
    /// Each session's last use, by [`LastUsed::key`].
    #[serde(default)]
    pub sessions: BTreeMap<String, u64>,
}

impl LastUsed {
    /// The record's key for `host`'s session `session_id`: the host's key
    /// (empty for this PC), a `/`, then the session id.
    #[must_use]
    pub fn key(host: Option<&str>, session_id: &str) -> String {
        format!("{}/{session_id}", host.unwrap_or_default())
    }

    /// Reads the record at `path`. A missing file is an empty record; one
    /// that cannot be read or parsed is logged and read as empty.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "phone: reading the last-used record");
                return Self::default();
            }
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            tracing::warn!(path = %path.display(), error = %err, "phone: reading the last-used record");
            Self::default()
        })
    }

    /// Writes the record to `path` in one step: the bytes land in a
    /// temporary file beside it and the rename swaps it in.
    ///
    /// # Errors
    ///
    /// Fails when the record cannot be serialized, its folder cannot be
    /// made, or the temporary file or the rename cannot be written.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Records the session `key` as used at `now`, keeping only the
    /// [`LAST_USED_MAX`] most recently used sessions; `key` is always kept.
    pub fn stamp(&mut self, key: &str, now: SystemTime) {
        let seconds = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        self.sessions.insert(key.to_owned(), seconds);
        while self.sessions.len() > LAST_USED_MAX {
            let oldest = self
                .sessions
                .iter()
                .filter(|(known, _)| known.as_str() != key)
                .min_by_key(|(known, at)| (**at, (*known).clone()))
                .map(|(known, _)| known.clone());
            let Some(oldest) = oldest else {
                break;
            };
            self.sessions.remove(&oldest);
        }
    }

    /// `keys` most recently used first; the ones never used follow in the
    /// order given.
    #[must_use]
    pub fn order(&self, keys: &[String]) -> Vec<String> {
        let mut ordered = keys.to_vec();
        ordered.sort_by_key(|key| std::cmp::Reverse(self.sessions.get(key).copied()));
        ordered
    }
}

/// Removes the file at `path`; one already gone is no error.
fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// `now` in milliseconds since the Unix epoch; a time before it is 0.
fn millis(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH).map_or(0, |since| {
        i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
    })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    const NOW_MS: u64 = 1_800_000_000_000;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("rt-native-inbox-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn at_ms(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    fn file_json(id: &str, created: u64) -> String {
        format!(
            r#"{{"v":1,"id":"{id}","kind":"file","name":"IMG_1.jpg","payload":"{id}.bin","text":null,"created":{created}}}"#
        )
    }

    /// Writes a file item `id` shared at `created`, with its payload when
    /// `payload` is set.
    fn write_file_item(dir: &Path, id: &str, created: u64, payload: bool) {
        if payload {
            std::fs::write(dir.join(format!("{id}.bin")), b"bytes").expect("write the payload");
        }
        std::fs::write(dir.join(format!("{id}.json")), file_json(id, created))
            .expect("write the manifest");
    }

    #[test]
    fn manifest_reads_file_text_and_url() {
        let file = Manifest::parse(file_json("a1", 5).as_bytes()).expect("a file manifest");
        assert_eq!(file.kind, Kind::File);
        assert_eq!(file.name.as_deref(), Some("IMG_1.jpg"));
        assert_eq!(file.payload.as_deref(), Some("a1.bin"));
        assert_eq!(file.created, 5);

        let text =
            Manifest::parse(br#"{"v":1,"id":"t1","kind":"text","text":"hello","created":7}"#)
                .expect("a text manifest");
        assert_eq!(text.kind, Kind::Text);
        assert_eq!(text.text.as_deref(), Some("hello"));
        assert_eq!(text.payload, None);

        let url = Manifest::parse(
            br#"{"v":1,"id":"u1","kind":"url","name":null,"payload":null,"text":"https://example.com/","created":9,"extra":true}"#,
        )
        .expect("a link manifest, an unknown field tolerated");
        assert_eq!(url.kind, Kind::Url);
        assert_eq!(url.text.as_deref(), Some("https://example.com/"));
    }

    #[test]
    fn items_the_share_extension_writes_read_back_ready() {
        use rustling_tulip_ios_share::{Inbox, TextKind};

        let dir = TestDir::new("extension");
        let source = TestDir::new("extension-source");
        std::fs::write(source.0.join("IMG_7.heic"), b"image bytes").expect("write the shared file");
        let inbox = Inbox::new(dir.0.clone());
        let file_id = inbox
            .write_file(&source.0.join("IMG_7.heic"), "IMG_7.heic", 10)
            .expect("write a file item");
        let text_id = inbox
            .write_text(TextKind::Text, "hello there", 20)
            .expect("write a text item");
        let url_id = inbox
            .write_text(TextKind::Url, "https://example.com/a?b=c", 30)
            .expect("write a link item");

        for id in [&file_id, &text_id, &url_id] {
            let bytes =
                read_manifest(&dir.0.join(format!("{id}.json"))).expect("the manifest reads");
            Manifest::parse(&bytes).expect("the manifest is valid");
        }
        let scan = scan(&dir.0, at_ms(NOW_MS));

        assert!(scan.bad.is_empty(), "{:?}", scan.bad);
        assert!(scan.stale.is_empty(), "{:?}", scan.stale);
        assert!(scan.orphans.is_empty(), "{:?}", scan.orphans);
        assert_eq!(scan.ready.len(), 3, "{:?}", scan.ready);
        let (file, text, url) = (&scan.ready[0], &scan.ready[1], &scan.ready[2]);
        assert_eq!(file.manifest.id, file_id);
        assert_eq!(file.manifest.kind, Kind::File);
        assert_eq!(file.manifest.name.as_deref(), Some("IMG_7.heic"));
        assert_eq!(file.manifest.created, 10);
        let payload = file
            .payload_path
            .as_deref()
            .expect("a file item names its payload");
        assert_eq!(
            std::fs::read(payload).expect("read the payload"),
            b"image bytes"
        );
        assert_eq!(text.manifest.id, text_id);
        assert_eq!(text.manifest.kind, Kind::Text);
        assert_eq!(text.manifest.text.as_deref(), Some("hello there"));
        assert_eq!(text.payload_path, None);
        assert_eq!(url.manifest.id, url_id);
        assert_eq!(url.manifest.kind, Kind::Url);
        assert_eq!(
            url.manifest.text.as_deref(),
            Some("https://example.com/a?b=c")
        );
        assert_eq!(url.payload_path, None);
    }

    #[test]
    fn manifest_refuses_unknown_kind_wrong_version_and_missing_fields() {
        let refused: [&[u8]; 7] = [
            br#"{"v":1,"id":"a","kind":"image","text":"x","created":1}"#,
            br#"{"v":2,"id":"a","kind":"text","text":"x","created":1}"#,
            br#"{"id":"a","kind":"text","text":"x","created":1}"#,
            br#"{"v":1,"kind":"text","text":"x","created":1}"#,
            br#"{"v":1,"id":"a","kind":"text","text":"x"}"#,
            br#"{"v":1,"id":"a","kind":"url","created":1}"#,
            br#"{"v":1,"id":"a","kind":"file","name":"x.jpg","created":1}"#,
        ];
        for bytes in refused {
            assert!(
                Manifest::parse(bytes).is_err(),
                "{} is refused",
                String::from_utf8_lossy(bytes)
            );
        }
        assert!(
            Manifest::parse(br#"{"v":1,"id":"a","kind":"file","payload":"a.bin","created":1}"#)
                .is_err(),
            "a file item without a name is refused"
        );
    }

    #[test]
    fn manifest_refuses_a_name_or_payload_with_a_path_separator_or_dotdot() {
        for bad in ["../x", "a/b", r"a\\b", "..", ".", ""] {
            let name = format!(
                r#"{{"v":1,"id":"a","kind":"file","name":{bad:?},"payload":"a.bin","created":1}}"#
            );
            assert!(
                Manifest::parse(name.as_bytes()).is_err(),
                "name {bad:?} is refused"
            );
            let payload = format!(
                r#"{{"v":1,"id":"a","kind":"file","name":"x.jpg","payload":{bad:?},"created":1}}"#
            );
            assert!(
                Manifest::parse(payload.as_bytes()).is_err(),
                "payload {bad:?} is refused"
            );
            let id = format!(r#"{{"v":1,"id":{bad:?},"kind":"text","text":"t","created":1}}"#);
            assert!(
                Manifest::parse(id.as_bytes()).is_err(),
                "id {bad:?} is refused"
            );
        }
        assert!(
            Manifest::parse(
                br#"{"v":1,"id":"a","kind":"file","name":"my..notes.txt","payload":"a.bin","created":1}"#
            )
            .is_ok(),
            "two dots inside a plain name are fine"
        );
    }

    #[test]
    fn scan_skips_a_file_item_whose_payload_is_missing() {
        let dir = TestDir::new("missing-payload");
        write_file_item(&dir.0, "waiting", NOW_MS - 1000, false);
        write_file_item(&dir.0, "here", NOW_MS - 1000, true);

        let scan = scan(&dir.0, at_ms(NOW_MS));

        let ids: Vec<&str> = scan
            .ready
            .iter()
            .map(|item| item.manifest.id.as_str())
            .collect();
        assert_eq!(ids, ["here"], "the item without its payload waits");
        assert!(scan.stale.is_empty(), "a fresh waiting item is not stale");
        assert_eq!(scan.bad, Vec::<PathBuf>::new());
        assert_eq!(scan.ready[0].payload_path, Some(dir.0.join("here.bin")));
        assert_eq!(scan.ready[0].json_path, dir.0.join("here.json"));
    }

    #[test]
    fn scan_marks_it_stale_after_a_day() {
        let dir = TestDir::new("stale");
        let day = u64::try_from(STALE_MS).expect("a day fits");
        write_file_item(&dir.0, "old", NOW_MS - day, false);
        write_file_item(&dir.0, "young", NOW_MS - day + 1, false);

        let scan = scan(&dir.0, at_ms(NOW_MS));

        assert_eq!(scan.ready, Vec::<Item>::new());
        assert_eq!(
            scan.stale,
            [dir.0.join("old.json")],
            "only the day-old item is stale"
        );
    }

    #[test]
    fn scan_ignores_tmp_and_non_json_files() {
        let dir = TestDir::new("ignored");
        std::fs::write(dir.0.join("a.json.tmp"), file_json("a", 1)).expect("write a tmp manifest");
        std::fs::write(dir.0.join("a.bin"), b"bytes").expect("write a payload");
        std::fs::write(dir.0.join("notes.txt"), b"not json").expect("write a text file");
        std::fs::create_dir_all(dir.0.join("folder.json")).expect("make a folder named json");

        let scan = scan(&dir.0, at_ms(NOW_MS));

        assert!(scan.ready.is_empty(), "{:?}", scan.ready);
        assert_eq!(scan.stale, Vec::<PathBuf>::new());
        assert!(scan.bad.is_empty(), "{:?}", scan.bad);
        assert!(
            super::scan(&dir.0.join("absent"), at_ms(NOW_MS))
                .ready
                .is_empty(),
            "a missing folder is an empty inbox"
        );
    }

    #[test]
    fn scan_marks_an_unreadable_manifest_bad() {
        let dir = TestDir::new("bad");
        std::fs::write(dir.0.join("broken.json"), b"{not json").expect("write a broken manifest");
        std::fs::write(
            dir.0.join("escape.json"),
            br#"{"v":1,"id":"escape","kind":"file","name":"x","payload":"../x.bin","created":1}"#,
        )
        .expect("write an escaping manifest");
        std::fs::write(dir.0.join("dir.json"), file_json("dir", 1)).expect("write a manifest");
        std::fs::create_dir_all(dir.0.join("dir.bin")).expect("make a folder as its payload");

        let scan = scan(&dir.0, at_ms(NOW_MS));

        let mut bad = scan.bad.clone();
        bad.sort();
        assert_eq!(
            bad,
            [
                dir.0.join("broken.json"),
                dir.0.join("dir.json"),
                dir.0.join("escape.json")
            ]
        );
        assert_eq!(scan.ready, Vec::<Item>::new());
    }

    #[test]
    fn scan_orders_ready_items_by_created() {
        let dir = TestDir::new("order");
        write_file_item(&dir.0, "c", 30, true);
        write_file_item(&dir.0, "b", 10, true);
        write_file_item(&dir.0, "a", 30, true);
        std::fs::write(
            dir.0.join("t.json"),
            br#"{"v":1,"id":"t","kind":"text","text":"hi","created":20}"#,
        )
        .expect("write a text manifest");

        let scan = scan(&dir.0, at_ms(NOW_MS));

        let ids: Vec<&str> = scan
            .ready
            .iter()
            .map(|item| item.manifest.id.as_str())
            .collect();
        assert_eq!(ids, ["b", "t", "a", "c"], "oldest first, then by id");
        assert_eq!(
            scan.ready[1].payload_path, None,
            "a text item names no payload"
        );
    }

    #[test]
    fn remove_deletes_the_payload_then_the_manifest() {
        let dir = TestDir::new("remove");
        write_file_item(&dir.0, "a", 1, true);
        let item = scan(&dir.0, at_ms(NOW_MS))
            .ready
            .pop()
            .expect("the item is ready");

        remove(&item).expect("remove the item");

        assert!(!dir.0.join("a.bin").exists(), "the payload is gone");
        assert!(!dir.0.join("a.json").exists(), "the manifest is gone");
        remove(&item).expect("removing it again is no error");

        let stuck = Item {
            payload_path: Some(dir.0.join("stuck.bin")),
            json_path: dir.0.join("stuck.json"),
            ..item
        };
        std::fs::create_dir_all(dir.0.join("stuck.bin"))
            .expect("a payload that cannot be removed as a file");
        std::fs::write(&stuck.json_path, b"{}").expect("write its manifest");

        assert!(remove(&stuck).is_err(), "the payload's delete fails");
        assert!(
            stuck.json_path.exists(),
            "the manifest stays while the payload does"
        );
    }

    #[test]
    fn inbox_dir_prefers_the_env_then_the_group_then_the_ui_dir() {
        let ui = std::env::temp_dir().join("ui");
        let env = std::env::temp_dir().join("inbox");
        let env_text = env.to_str().expect("a UTF-8 temp path");
        let group = std::env::temp_dir().join("group").join(GROUP_FOLDER);

        assert_eq!(inbox_dir(Some(env_text), Some(group.clone()), &ui), env);
        assert_eq!(inbox_dir(None, Some(group.clone()), &ui), group);
        assert_eq!(inbox_dir(None, None, &ui), ui.join(UI_FOLDER));
    }

    #[test]
    fn inbox_dir_ignores_a_relative_env_value() {
        let ui = std::env::temp_dir().join("ui");
        let group = std::env::temp_dir().join("group").join(GROUP_FOLDER);

        assert_eq!(
            inbox_dir(Some("relative/inbox"), None, &ui),
            ui.join(UI_FOLDER)
        );
        assert_eq!(inbox_dir(Some(""), Some(group.clone()), &ui), group);
    }

    #[test]
    fn manifest_refuses_a_drive_relative_name_or_payload() {
        for bad in ["D:victim.bin", "C:", "a:b"] {
            let name = format!(
                r#"{{"v":1,"id":"a","kind":"file","name":{bad:?},"payload":"a.bin","created":1}}"#
            );
            assert!(
                Manifest::parse(name.as_bytes()).is_err(),
                "name {bad:?} is refused"
            );
            let payload = format!(
                r#"{{"v":1,"id":"a","kind":"file","name":"x.jpg","payload":{bad:?},"created":1}}"#
            );
            assert!(
                Manifest::parse(payload.as_bytes()).is_err(),
                "payload {bad:?} is refused"
            );
            let id = format!(r#"{{"v":1,"id":{bad:?},"kind":"text","text":"t","created":1}}"#);
            assert!(
                Manifest::parse(id.as_bytes()).is_err(),
                "id {bad:?} is refused"
            );
        }
    }

    #[test]
    fn a_manifest_whose_name_is_not_its_id_is_bad() {
        let dir = TestDir::new("name-not-id");
        std::fs::write(dir.0.join("a.bin"), b"bytes").expect("write the payload");
        std::fs::write(dir.0.join("other.json"), file_json("a", 1)).expect("write the manifest");

        let scan = scan(&dir.0, at_ms(NOW_MS));

        assert_eq!(scan.ready, Vec::<Item>::new());
        assert_eq!(scan.bad, [dir.0.join("other.json")]);
    }

    #[test]
    fn a_file_item_whose_payload_is_not_its_id_bin_is_bad() {
        let dir = TestDir::new("payload-not-id");
        std::fs::write(dir.0.join("b.bin"), b"bytes").expect("write another payload");
        std::fs::write(
            dir.0.join("a.json"),
            br#"{"v":1,"id":"a","kind":"file","name":"x.jpg","payload":"b.bin","created":1}"#,
        )
        .expect("write the manifest");

        let scan = scan(&dir.0, at_ms(NOW_MS));

        assert_eq!(scan.ready, Vec::<Item>::new());
        assert_eq!(scan.bad, [dir.0.join("a.json")]);
    }

    #[test]
    fn a_text_item_with_a_payload_is_bad() {
        assert!(
            Manifest::parse(
                br#"{"v":1,"id":"t","kind":"text","text":"hi","payload":"t.bin","created":1}"#
            )
            .is_err(),
            "a text item names no payload"
        );
        assert!(
            Manifest::parse(br#"{"v":1,"id":"u","kind":"url","text":"https://x.test/","payload":"u.bin","created":1}"#)
                .is_err(),
            "a link item names no payload"
        );
    }

    #[test]
    fn removing_a_text_item_deletes_only_its_manifest() {
        let dir = TestDir::new("remove-text");
        std::fs::write(dir.0.join("t.bin"), b"someone else's").expect("write a stray payload");
        std::fs::write(
            dir.0.join("t.json"),
            br#"{"v":1,"id":"t","kind":"text","text":"hi","created":1}"#,
        )
        .expect("write the manifest");
        let item = scan(&dir.0, at_ms(NOW_MS))
            .ready
            .pop()
            .expect("the text item is ready");
        assert_eq!(item.payload_path, None);

        remove(&item).expect("remove the text item");

        assert!(!dir.0.join("t.json").exists(), "the manifest is gone");
        assert!(
            dir.0.join("t.bin").exists(),
            "no payload is deleted for text"
        );
    }

    #[test]
    fn a_manifest_over_the_cap_is_bad() {
        let dir = TestDir::new("over-cap");
        let text = "x".repeat(1024 * 1024);
        let json = format!(r#"{{"v":1,"id":"big","kind":"text","text":"{text}","created":1}}"#);
        std::fs::write(dir.0.join("big.json"), json).expect("write a large manifest");

        let scan = scan(&dir.0, at_ms(NOW_MS));

        assert_eq!(scan.ready, Vec::<Item>::new());
        assert_eq!(scan.bad, [dir.0.join("big.json")]);
    }

    #[test]
    fn last_used_orders_most_recent_first_and_keeps_the_rest() {
        let keys: Vec<String> = ["h/a", "h/b", "h/c", "h/d", "/e"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut record = LastUsed::default();
        record.stamp("h/c", at_ms(NOW_MS));
        record.stamp("h/a", at_ms(NOW_MS + 5_000));
        record.stamp("gone/x", at_ms(NOW_MS + 9_000));

        assert_eq!(record.order(&keys), ["h/a", "h/c", "h/b", "h/d", "/e"]);
        assert_eq!(LastUsed::key(Some("ssh:me@box"), "s1"), "ssh:me@box/s1");
        assert_eq!(LastUsed::key(None, "s1"), "/s1");
    }

    #[test]
    fn scan_lists_an_orphan_payload_after_a_day() {
        let dir = TestDir::new("orphans");
        let day = Duration::from_millis(u64::try_from(STALE_MS).expect("a day fits"));
        let now = at_ms(NOW_MS);
        let aged = |name: &str, age: Duration| {
            let path = dir.0.join(name);
            std::fs::write(&path, b"bytes").expect("write a payload");
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| {
                    file.set_modified(now.checked_sub(age).expect("a time after the epoch"))
                })
                .expect("age the payload");
        };
        aged("old.bin", day);
        aged("young.bin", day.saturating_sub(Duration::from_secs(1)));
        aged("named.bin", day * 2);
        std::fs::write(dir.0.join("named.json"), file_json("named", 1))
            .expect("write its manifest");

        let scan = scan(&dir.0, now);

        assert_eq!(
            scan.orphans,
            [dir.0.join("old.bin")],
            "only the day-old payload with no manifest is an orphan"
        );
        let ids: Vec<&str> = scan
            .ready
            .iter()
            .map(|item| item.manifest.id.as_str())
            .collect();
        assert_eq!(ids, ["named"], "a payload with its manifest is an item");
    }

    #[test]
    fn last_used_keeps_the_50_most_recent() {
        let mut record = LastUsed::default();
        for n in 0..60_u64 {
            record.stamp(&format!("h/s{n}"), at_ms(NOW_MS + n * 1000));
        }

        assert_eq!(record.sessions.len(), LAST_USED_MAX);
        assert!(
            (10..60).all(|n| record.sessions.contains_key(&format!("h/s{n}"))),
            "the 50 most recent stay"
        );
        assert!(!record.sessions.contains_key("h/s9"), "the older ones go");

        record.stamp("h/new", at_ms(NOW_MS));
        assert!(
            record.sessions.contains_key("h/new"),
            "the key just stamped stays even when it ties the oldest"
        );
        assert_eq!(record.sessions.len(), LAST_USED_MAX);
    }

    #[test]
    fn last_used_round_trips_and_survives_a_corrupt_file() {
        let dir = TestDir::new("last-used");
        let path = dir.0.join(LAST_USED_FILE);
        assert_eq!(
            LastUsed::load(&path),
            LastUsed::default(),
            "a missing file is empty"
        );

        let mut record = LastUsed::default();
        record.stamp("h/a", at_ms(NOW_MS));
        record.save(&path).expect("stamp and save");
        let read = LastUsed::load(&path);
        assert_eq!(read.sessions.get("h/a"), Some(&(NOW_MS / 1000)));
        assert!(
            !dir.0.join("recent-sessions.json.tmp").exists(),
            "no temp file is left"
        );

        std::fs::write(&path, b"{corrupt").expect("corrupt the record");
        assert_eq!(
            LastUsed::load(&path),
            LastUsed::default(),
            "a corrupt file is empty"
        );
        let mut record = LastUsed::load(&path);
        record.stamp("h/b", at_ms(NOW_MS));
        record.save(&path).expect("a corrupt file is replaced");
        assert_eq!(LastUsed::load(&path).sessions.len(), 1);
    }
}
