//! The phone app's share extension, as plain Rust: how a shared file, text
//! or link lands in the app's share inbox (`apps/native/src/phone/inbox.rs`
//! reads it). The iOS binary (`src/main.rs`) wraps it in the extension
//! class.
//!
//! Each item is a manifest `{id}.json` and, for a file, its payload
//! `{id}.bin`. The payload is written first, then the manifest lands by
//! renaming `{id}.json.tmp`, so the app never reads a manifest whose payload
//! is still being written. Nothing a shared item says names a path: a file's
//! name goes only into the manifest's `name`, made one plain file name.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// The App Group the phone app and its share extension share.
pub const GROUP_ID: &str = "group.dev.leftos.rustling-tulip.phone";

/// The inbox folder inside the App Group container.
pub const GROUP_FOLDER: &str = "ShareInbox";

/// The largest file the extension writes: 64 MiB; a larger one is skipped.
pub const FILE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The most a manifest may hold: 1 MiB, the most the app reads.
pub const MANIFEST_MAX_BYTES: usize = 1024 * 1024;

/// The manifest version the app reads.
const MANIFEST_VERSION: u32 = 1;

/// The type a shared link conforms to.
pub const URL_TYPE: &str = "public.url";

/// The type a shared file's location conforms to.
pub const FILE_URL_TYPE: &str = "public.file-url";

/// The type plain text conforms to.
pub const TEXT_TYPE: &str = "public.plain-text";

/// The types that make a provider a link or text rather than a file:
/// these, and the `public.utf8` and `public.utf16` text variants.
const TEXT_OR_URL_TYPES: [&str; 3] = [URL_TYPE, TEXT_TYPE, "public.text"];

/// The URL the extension asks iOS to open once it has written a shared
/// item, which brings the app to the front; its scheme is the one
/// apps/ios/Info.plist registers.
pub const OPEN_APP_URL: &str = "rustling-tulip://share";

/// How long the extension waits for its providers before it completes the
/// request anyway: 30 s.
pub const DEADLINE_SECS: u64 = 30;

/// What a provider's item is written as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// A file, copied from the provider's file representation.
    File,
    /// A link.
    Url,
    /// Plain text.
    Text,
}

/// A shared file's name when its own is empty or not a name at all.
const FALLBACK_NAME: &str = "shared";

/// What a shared item is, as the manifest's `kind` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A file: its bytes are the payload.
    File,
    /// Plain text, held in the manifest.
    Text,
    /// A link, held in the manifest.
    Url,
}

/// A text item's kind: plain text or a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    /// Plain text.
    Text,
    /// A link.
    Url,
}

/// One shared item, as its manifest describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item<'a> {
    /// A file named `name`, already one plain file name.
    File {
        /// The file's name.
        name: &'a str,
    },
    /// Plain text.
    Text(&'a str),
    /// A link.
    Url(&'a str),
}

/// Why an item was not written.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The file is over [`FILE_MAX_BYTES`]; it holds this many bytes.
    #[error("the file is {0} bytes, over the {FILE_MAX_BYTES}-byte cap")]
    TooLarge(u64),
    /// The manifest would be over [`MANIFEST_MAX_BYTES`]; it would hold
    /// this many bytes.
    #[error("the manifest is {0} bytes, over the {MANIFEST_MAX_BYTES}-byte cap")]
    ManifestTooLarge(usize),
    /// A file could not be read or written.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The manifest could not be serialized.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// One manifest as the app reads it.
#[derive(Serialize)]
struct Manifest<'a> {
    v: u32,
    id: &'a str,
    kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    created: i64,
}

/// What a provider registering `types` is written as: a file when it has
/// a file URL or any type that is not a link or text type; else a link
/// when it has `public.url`; else text when it has `public.plain-text`;
/// else nothing.
#[must_use]
pub fn classify(types: &[&str]) -> Option<ItemKind> {
    if types
        .iter()
        .any(|kind| *kind == FILE_URL_TYPE || !is_text_or_url(kind))
    {
        Some(ItemKind::File)
    } else if types.contains(&URL_TYPE) {
        Some(ItemKind::Url)
    } else if types.contains(&TEXT_TYPE) {
        Some(ItemKind::Text)
    } else {
        None
    }
}

/// The type a file provider's file is loaded as: its first registered type
/// that is neither a file URL nor a link or text type; `None` when it has
/// only those, so its file URL is loaded instead.
#[must_use]
pub fn file_type<'a>(types: &[&'a str]) -> Option<&'a str> {
    types
        .iter()
        .copied()
        .find(|kind| *kind != FILE_URL_TYPE && !is_text_or_url(kind))
}

/// Whether `kind` is one of the link or text types.
fn is_text_or_url(kind: &str) -> bool {
    TEXT_OR_URL_TYPES.contains(&kind)
        || kind.starts_with("public.utf8")
        || kind.starts_with("public.utf16")
}

/// The providers still to answer and whether the request is complete: the
/// request completes once, when the last provider answers or the deadline
/// passes, whichever comes first.
#[derive(Debug)]
pub struct Countdown {
    left: AtomicUsize,
    done: AtomicBool,
}

impl Countdown {
    /// A countdown over `providers` answers.
    #[must_use]
    pub fn new(providers: usize) -> Self {
        Self {
            left: AtomicUsize::new(providers),
            done: AtomicBool::new(false),
        }
    }

    /// Whether the request is complete, so an answer writes nothing.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// One provider has answered; true when it was the last and the
    /// request is not yet complete, so the caller completes it now.
    pub fn answer(&self) -> bool {
        // An answer past the last wraps the count, which never reads 1 again
        // before the done flag has long been set.
        let before = self.left.fetch_sub(1, Ordering::AcqRel);
        before == 1 && self.close()
    }

    /// The deadline passed (or there is nothing to wait for); true when the
    /// request is not yet complete, so the caller completes it now.
    pub fn expire(&self) -> bool {
        self.close()
    }

    /// Marks the request complete; true only for the first call.
    fn close(&self) -> bool {
        !self.done.swap(true, Ordering::AcqRel)
    }
}

/// Whether the share wrote at least one item into the inbox, so the
/// extension asks iOS to open the app once it completes.
#[derive(Debug, Default)]
pub struct Written(AtomicBool);

impl Written {
    /// Notes that an item was written into the inbox.
    pub fn record(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether an item was written into the inbox.
    #[must_use]
    pub fn any(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Whether a file of `len` bytes is written: at most [`FILE_MAX_BYTES`].
#[must_use]
pub fn fits(len: u64) -> bool {
    len <= FILE_MAX_BYTES
}

/// A fresh item id: a random UUID as 32 lowercase hex digits, one plain
/// file name.
#[must_use]
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The time now in milliseconds since the Unix epoch, as a manifest's
/// `created` holds it; 0 for a clock before the epoch.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// `name` as one plain file name the app accepts: each `/`, `\\`, `:` and
/// NUL becomes `_`, and an empty name, `.` or `..` becomes `shared`.
#[must_use]
pub fn plain_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if matches!(ch, '/' | '\\' | ':' | '\0') {
                '_'
            } else {
                ch
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        FALLBACK_NAME.to_owned()
    } else {
        cleaned
    }
}

/// The manifest bytes for `item` with id `id`, shared at `created`
/// milliseconds: a file names its payload `{id}.bin` and holds no text; text
/// and a link hold their text and name no payload.
///
/// # Errors
///
/// Fails when the manifest would be over [`MANIFEST_MAX_BYTES`].
pub fn manifest_json(id: &str, item: Item<'_>, created: i64) -> Result<Vec<u8>, WriteError> {
    let (kind, name, text) = match item {
        Item::File { name } => (Kind::File, Some(name), None),
        Item::Text(text) => (Kind::Text, None, Some(text)),
        Item::Url(text) => (Kind::Url, None, Some(text)),
    };
    let manifest = Manifest {
        v: MANIFEST_VERSION,
        id,
        kind,
        name,
        payload: (kind == Kind::File).then(|| format!("{id}.bin")),
        text,
        created,
    };
    let bytes = serde_json::to_vec(&manifest)?;
    if bytes.len() > MANIFEST_MAX_BYTES {
        return Err(WriteError::ManifestTooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// The share inbox folder items are written into.
#[derive(Clone, Debug)]
pub struct Inbox {
    dir: PathBuf,
}

impl Inbox {
    /// The inbox at `dir`, made when the first item is written.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The inbox folder.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes a text or link item holding `text`, shared at `created`
    /// milliseconds, and returns its id.
    ///
    /// # Errors
    ///
    /// Fails when the manifest would be over [`MANIFEST_MAX_BYTES`] or
    /// cannot be written; nothing is left behind.
    pub fn write_text(
        &self,
        kind: TextKind,
        text: &str,
        created: i64,
    ) -> Result<String, WriteError> {
        let id = new_id();
        let item = match kind {
            TextKind::Text => Item::Text(text),
            TextKind::Url => Item::Url(text),
        };
        let json = manifest_json(&id, item, created)?;
        fs::create_dir_all(&self.dir)?;
        self.place_manifest(&id, &json)?;
        Ok(id)
    }

    /// Writes the file at `source`, shared as `name` at `created`
    /// milliseconds, and returns its id: its bytes are copied into the
    /// payload, then the manifest lands.
    ///
    /// # Errors
    ///
    /// Fails with [`WriteError::TooLarge`] for a file over
    /// [`FILE_MAX_BYTES`], which writes nothing, and when the file cannot be
    /// read or the item written; nothing is left behind.
    pub fn write_file(
        &self,
        source: &Path,
        name: &str,
        created: i64,
    ) -> Result<String, WriteError> {
        let len = fs::metadata(source)?.len();
        if !fits(len) {
            return Err(WriteError::TooLarge(len));
        }
        self.write_with(name, created, |payload| copy_fresh(source, payload))
    }

    /// Writes a file item shared as `name` at `created` milliseconds whose
    /// payload `fill` writes at the path it is given, and returns its id. The
    /// manifest lands only after `fill` returns.
    ///
    /// # Errors
    ///
    /// Fails when the manifest would be over [`MANIFEST_MAX_BYTES`], when
    /// `fill` fails, or when the manifest cannot be written; the payload is
    /// then removed.
    pub fn write_with<F>(&self, name: &str, created: i64, fill: F) -> Result<String, WriteError>
    where
        F: FnOnce(&Path) -> io::Result<()>,
    {
        let id = new_id();
        let name = plain_name(name);
        let json = manifest_json(&id, Item::File { name: &name }, created)?;
        fs::create_dir_all(&self.dir)?;
        let payload = self.dir.join(format!("{id}.bin"));
        let placed = fill(&payload)
            .map_err(WriteError::from)
            .and_then(|()| self.place_manifest(&id, &json));
        if let Err(err) = placed {
            remove_quietly(&payload);
            return Err(err);
        }
        Ok(id)
    }

    /// Lands `json` as `{id}.json`: written whole to `{id}.json.tmp`, then
    /// renamed; a failure removes the temporary file.
    fn place_manifest(&self, id: &str, json: &[u8]) -> Result<(), WriteError> {
        let tmp = self.dir.join(format!("{id}.json.tmp"));
        let landed = self.dir.join(format!("{id}.json"));
        let written = write_synced(&tmp, json).and_then(|()| fs::rename(&tmp, &landed));
        if let Err(err) = written {
            remove_quietly(&tmp);
            return Err(err.into());
        }
        Ok(())
    }
}

/// Copies the file at `source` into a new file at `dest` and flushes it
/// to disk; the new file's times are its own, so a fresh payload never looks
/// a day old to the app's orphan sweep.
fn copy_fresh(source: &Path, dest: &Path) -> io::Result<()> {
    let mut from = fs::File::open(source)?;
    let mut to = fs::File::create(dest)?;
    io::copy(&mut from, &mut to)?;
    to.sync_all()
}

/// Writes `bytes` to a new file at `path` and flushes it to disk.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Removes the file at `path`, if there is one; a failure is passed over,
/// as the item already failed.
fn remove_quietly(path: &Path) {
    drop(fs::remove_file(path));
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::{Value, json};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("rt-ios-share-{}-{tag}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .expect("list the test dir")
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn parsed(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("the manifest is JSON")
    }

    #[test]
    fn classify_picks_each_providers_kind() {
        let rows: [(&[&str], Option<ItemKind>); 6] = [
            (&["public.url"], Some(ItemKind::Url)),
            (&["public.plain-text", "public.text"], Some(ItemKind::Text)),
            (
                &["public.plain-text", "public.file-url"],
                Some(ItemKind::File),
            ),
            (&["public.jpeg", "public.image"], Some(ItemKind::File)),
            (&["public.file-url"], Some(ItemKind::File)),
            (&[], None),
        ];
        for (types, kind) in rows {
            assert_eq!(classify(types), kind, "{types:?}");
        }
        assert_eq!(file_type(&["public.plain-text", "public.file-url"]), None);
        assert_eq!(
            file_type(&["public.file-url", "com.adobe.pdf"]),
            Some("com.adobe.pdf")
        );
        assert_eq!(
            classify(&["public.utf8-plain-text", "public.plain-text"]),
            Some(ItemKind::Text),
            "a text variant is still text"
        );
    }

    #[test]
    fn countdown_completes_once_on_the_last_answer() {
        let countdown = Countdown::new(2);
        assert!(!countdown.answer(), "one provider is still to answer");
        assert!(!countdown.is_done());
        assert!(countdown.answer(), "the last answer completes");
        assert!(countdown.is_done());
        assert!(!countdown.expire(), "the deadline does not complete again");
        assert!(
            !countdown.answer(),
            "an extra answer does not complete again"
        );
    }

    #[test]
    fn countdown_deadline_completes_before_the_last_answer() {
        let countdown = Countdown::new(2);
        assert!(!countdown.answer());
        assert!(countdown.expire(), "the deadline completes");
        assert!(countdown.is_done(), "later answers write nothing");
        assert!(
            !countdown.answer(),
            "the last answer does not complete again"
        );
        assert!(!countdown.expire());
    }

    #[test]
    fn a_copied_payload_is_fresh_however_old_its_source() {
        let dir = TestDir::new("fresh");
        let source = TestDir::new("fresh-source");
        let path = source.0.join("old.jpg");
        fs::write(&path, b"old bytes").expect("write the source");
        let two_days = std::time::Duration::from_hours(48);
        let then = SystemTime::now()
            .checked_sub(two_days)
            .expect("a time after the epoch");
        fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(then))
            .expect("age the source");
        let inbox = Inbox::new(dir.0.clone());

        let id = inbox
            .write_file(&path, "old.jpg", 1)
            .expect("write the file item");

        let modified = fs::metadata(dir.0.join(format!("{id}.bin")))
            .and_then(|meta| meta.modified())
            .expect("read the payload's mtime");
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        assert!(
            age < std::time::Duration::from_secs(60),
            "the payload is {age:?} old"
        );
    }

    #[test]
    fn manifest_json_has_each_kinds_shape() {
        let file =
            manifest_json("a1", Item::File { name: "IMG_1.heic" }, 5).expect("a file manifest");
        assert_eq!(
            parsed(&file),
            json!({"v": 1, "id": "a1", "kind": "file", "name": "IMG_1.heic", "payload": "a1.bin", "created": 5})
        );

        let text = manifest_json("t1", Item::Text("hello"), 7).expect("a text manifest");
        assert_eq!(
            parsed(&text),
            json!({"v": 1, "id": "t1", "kind": "text", "text": "hello", "created": 7})
        );

        let url =
            manifest_json("u1", Item::Url("https://example.com/"), 9).expect("a link manifest");
        assert_eq!(
            parsed(&url),
            json!({"v": 1, "id": "u1", "kind": "url", "text": "https://example.com/", "created": 9})
        );
    }

    #[test]
    fn a_manifest_over_the_cap_is_not_written() {
        let dir = TestDir::new("over-cap");
        let inbox = Inbox::new(dir.0.clone());
        let text = "x".repeat(MANIFEST_MAX_BYTES);

        let written = inbox.write_text(TextKind::Text, &text, 1);

        assert!(
            matches!(written, Err(WriteError::ManifestTooLarge(_))),
            "{written:?}"
        );
        assert_eq!(dir.names(), Vec::<String>::new(), "nothing is left behind");
    }

    #[test]
    fn plain_name_makes_one_file_name() {
        assert_eq!(plain_name("IMG_1.heic"), "IMG_1.heic");
        assert_eq!(plain_name("../a/b\\c:d\0e"), ".._a_b_c_d_e");
        assert_eq!(plain_name(""), "shared");
        assert_eq!(plain_name("."), "shared");
        assert_eq!(plain_name(".."), "shared");
    }

    #[test]
    fn a_file_items_payload_lands_before_its_manifest() {
        let dir = TestDir::new("order");
        let inbox = Inbox::new(dir.0.clone());

        let id = inbox
            .write_with("a/b.pdf", 3, |payload| {
                assert_eq!(
                    dir.names(),
                    Vec::<String>::new(),
                    "no manifest exists while the payload is written"
                );
                fs::write(payload, b"pdf bytes")
            })
            .expect("write the file item");

        assert_eq!(
            dir.names(),
            [format!("{id}.bin"), format!("{id}.json")],
            "no .tmp is left"
        );
        let manifest =
            parsed(&fs::read(dir.0.join(format!("{id}.json"))).expect("read the manifest"));
        assert_eq!(manifest["name"], "a_b.pdf");
        assert_eq!(manifest["payload"], format!("{id}.bin"));
        assert_eq!(
            fs::read(dir.0.join(format!("{id}.bin"))).expect("read the payload"),
            b"pdf bytes"
        );
    }

    #[test]
    fn a_failed_payload_leaves_no_manifest_and_no_payload() {
        let dir = TestDir::new("failed");
        let inbox = Inbox::new(dir.0.clone());

        let written = inbox.write_with("x.jpg", 3, |payload| {
            fs::write(payload, b"half")?;
            Err(io::Error::other("the copy broke"))
        });
        let missing = inbox.write_file(&dir.0.join("absent.jpg"), "absent.jpg", 3);

        assert!(written.is_err(), "{written:?}");
        assert!(matches!(missing, Err(WriteError::Io(_))), "{missing:?}");
        assert_eq!(dir.names(), Vec::<String>::new(), "nothing is left behind");
    }

    #[test]
    fn open_app_url_uses_the_scheme_the_app_registers() {
        let plist = include_str!("../../ios/Info.plist");
        let key = plist
            .find("<key>CFBundleURLSchemes</key>")
            .expect("the app registers URL schemes");
        let rest = &plist[key..];
        let end = rest.find("</array>").expect("the schemes array closes");
        assert!(
            rest[..end].contains("<string>rustling-tulip</string>"),
            "the app registers the rustling-tulip scheme"
        );
        assert!(
            OPEN_APP_URL.starts_with("rustling-tulip://"),
            "{OPEN_APP_URL}"
        );
    }

    #[test]
    fn written_is_false_until_an_item_is_recorded() {
        let written = Written::default();
        assert!(!written.any(), "nothing is written yet");
        written.record();
        assert!(written.any(), "an item was written");
        written.record();
        assert!(written.any(), "a second item keeps it set");
    }

    #[test]
    fn a_file_over_64_mib_is_skipped() {
        assert!(fits(0));
        assert!(fits(FILE_MAX_BYTES));
        assert!(!fits(FILE_MAX_BYTES + 1));
        assert_eq!(FILE_MAX_BYTES, 64 * 1024 * 1024);
    }

    #[test]
    fn write_file_copies_the_source_and_text_lands_alone() {
        let dir = TestDir::new("copy");
        let source = TestDir::new("copy-source");
        fs::write(source.0.join("note.txt"), b"note").expect("write the source");
        let inbox = Inbox::new(dir.0.join("ShareInbox"));

        let file = inbox
            .write_file(&source.0.join("note.txt"), "note.txt", 4)
            .expect("write the file item");
        let link = inbox
            .write_text(TextKind::Url, "https://example.com/", 5)
            .expect("write the link item");

        assert_eq!(
            fs::read(inbox.dir().join(format!("{file}.bin"))).expect("read the payload"),
            b"note"
        );
        assert!(inbox.dir().join(format!("{link}.json")).is_file());
        assert!(
            !inbox.dir().join(format!("{link}.bin")).exists(),
            "a link has no payload"
        );
        assert_ne!(file, link, "each item has its own id");
        assert_eq!(file.len(), 32);
        assert!(file.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
