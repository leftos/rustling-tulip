//! The folder fetch's walk: which files of a confined folder
//! ([`crate::file_fetch::confine_dir`]) a `FetchFolder` sends, within its
//! [`Limits`], and what a `StatPath` path names ([`stat`]); and its pack
//! ([`pack`]): those files as one zstd-compressed tar, sent as `FileChunk`
//! frames.
//!
//! The walk follows no link. It reads each folder with one `read_dir` and
//! joins the names it yields to the verbatim (`\\?\`) canonical root, so a
//! name with a trailing dot or space, a reserved name such as `CON`, and a
//! path past `MAX_PATH` all name exactly their own entry. Types come from
//! the directory entry, never following a link; sizes come from
//! `symlink_metadata`, read from a handle, since the directory entry's size
//! is stale for a file held open for writing.
//!
//! The walk is a snapshot: the pack opens each listed file again and checks
//! where its handle really is ([`crate::file_tail::open_inside`]) before
//! reading a byte. On Unix that check has a gap: a file swapped for a FIFO
//! between the walk and the open can make the open block before the
//! handle's type is checked, and on macOS, which has no `/proc/self/fd`, a
//! handle's final path is its opened path canonicalized again, which a swap
//! between the open and that check can fool.

use crate::file_fetch::{self, CHUNK_SIZE, CanonicalRoot};
use crate::file_tail::{self, Folders, StepError};
use base64::Engine as _;
use protocol::{DaemonMessage, PathKind};
use std::fs::FileType;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// The folders a folder fetch leaves out, matched against an entry's whole
/// name, ASCII case-insensitively, at any depth below the fetch root (never
/// the fetch root itself). `.git` is left out whatever its type; the others
/// only as folders. Build outputs (`dist`, `build`, `out`) are kept.
pub const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".gradle",
    ".tmp",
    ".cache",
    ".parcel-cache",
    ".next",
    ".nuxt",
    ".turbo",
    ".svelte-kit",
    ".angular",
];

/// Whether the entry `name`, of `file_type`, is one [`IGNORED_DIRS`] leaves
/// out.
fn is_ignored(name: &str, file_type: Option<FileType>) -> bool {
    if name.eq_ignore_ascii_case(".git") {
        return true;
    }
    file_type.is_some_and(|kind| kind.is_dir() && !kind.is_symlink())
        && IGNORED_DIRS
            .iter()
            .any(|ignored| ignored.eq_ignore_ascii_case(name))
}

/// How much one walk may take on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Files listed.
    pub files: u64,
    /// Their total size in bytes.
    pub bytes: u64,
    /// Entries looked at: files, folders and skipped entries alike (an
    /// ignored folder counts toward no limit).
    pub visits: u64,
}

impl Limits {
    /// A `FetchFolder`'s limits.
    pub const FETCH: Self = Self {
        files: 20_000,
        bytes: 512 * 1024 * 1024,
        visits: 200_000,
    };
}

/// One file the walk listed.
#[derive(Debug)]
pub struct ScannedFile {
    /// Its path relative to the fetch root, `/`-separated, with no prefix.
    pub name: String,
    /// Its verbatim path on disk.
    pub path: PathBuf,
    /// Its size when the walk saw it.
    pub size: u64,
}

/// What a walk found.
#[derive(Debug, Default)]
pub struct Scan {
    /// The files to send, in walk order.
    pub files: Vec<ScannedFile>,
    /// Their total size in bytes.
    pub bytes: u64,
    /// The names of the ignored folders left out, each once, in the order
    /// first met: for the client to say what was skipped.
    pub skipped: Vec<String>,
    /// Entries left out for what they are: links, entries neither file nor
    /// folder, names that are not Unicode, and folders or entries that could
    /// not be read.
    pub unlisted: u64,
}

/// Why a walk gave up.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("the folder has more than {limit} files")]
    Files { limit: u64 },
    #[error("the folder holds more than {}", size_text(*limit))]
    Bytes { limit: u64 },
    #[error("the folder has more than {limit} entries to look through")]
    Visits { limit: u64 },
    #[error("the folder fetch was cancelled")]
    Cancelled,
    #[error("reading the folder {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// `bytes` as a limit reads: whole MiB where it is some, else bytes.
fn size_text(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// Walk `root` and list the files a folder fetch sends: every regular file
/// below it, but those under an ignored folder, following no link. Stops at
/// the first entry past one of `limits`, and at the first entry after
/// `cancel` fires. Disk work: run it on a blocking thread.
///
/// # Errors
/// The limit passed, the cancel, or the root itself failing to read. A
/// folder below the root that fails to read is counted in
/// [`Scan::unlisted`], not an error.
pub fn scan(
    root: &CanonicalRoot,
    limits: Limits,
    cancel: &CancellationToken,
) -> Result<Scan, ScanError> {
    let mut walk = Walk {
        found: Scan::default(),
        files: 0,
        visits: 0,
        limits,
        pending: vec![(root.path().to_path_buf(), String::new())],
    };
    while let Some((dir, prefix)) = walk.pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if prefix.is_empty() => {
                return Err(ScanError::Read {
                    path: dir.display().to_string(),
                    source,
                });
            }
            Err(err) => {
                debug!(folder = %dir.display(), error = %err, "folder_pack: a folder does not read");
                walk.found.unlisted += 1;
                continue;
            }
        };
        for entry in entries {
            if cancel.is_cancelled() {
                return Err(ScanError::Cancelled);
            }
            walk.take(classify(&dir, &prefix, entry))?;
        }
    }
    Ok(walk.found)
}

/// A walk under way: what it found, and what it has counted against its
/// limits.
struct Walk {
    found: Scan,
    files: u64,
    visits: u64,
    limits: Limits,
    /// Folders still to read: their path, and their name relative to the
    /// root ("" for the root).
    pending: Vec<(PathBuf, String)>,
}

impl Walk {
    /// Count one classified entry and file it.
    ///
    /// # Errors
    /// The entry is past one of the limits.
    fn take(&mut self, kind: Kind) -> Result<(), ScanError> {
        let visit = match kind {
            Kind::Ignored(name) => {
                if !self.found.skipped.contains(&name) {
                    self.found.skipped.push(name);
                }
                return Ok(());
            }
            Kind::Counted(visit) => visit,
        };
        self.visits += 1;
        if self.visits > self.limits.visits {
            return Err(ScanError::Visits {
                limit: self.limits.visits,
            });
        }
        match visit {
            Visit::Unlisted => self.found.unlisted += 1,
            Visit::Dir(path, rel) => self.pending.push((path, rel)),
            Visit::File(file) => self.list(file)?,
        }
        Ok(())
    }

    /// List `file`, unless it takes the walk past its file or byte limit.
    fn list(&mut self, file: ScannedFile) -> Result<(), ScanError> {
        if self.files >= self.limits.files {
            return Err(ScanError::Files {
                limit: self.limits.files,
            });
        }
        let bytes = self.found.bytes.saturating_add(file.size);
        if bytes > self.limits.bytes {
            return Err(ScanError::Bytes {
                limit: self.limits.bytes,
            });
        }
        self.files += 1;
        self.found.bytes = bytes;
        self.found.files.push(file);
        Ok(())
    }
}

/// What one directory entry is to the walk.
enum Kind {
    /// An ignored folder, by name: counted toward no limit.
    Ignored(String),
    /// Anything else, counted as a visit.
    Counted(Visit),
}

/// What a counted entry is to the walk.
enum Visit {
    /// Left out for what it is: counted in [`Scan::unlisted`].
    Unlisted,
    /// A folder to read: its path, and its name relative to the root.
    Dir(PathBuf, String),
    /// A regular file to list.
    File(ScannedFile),
}

/// What `entry`, read from the folder `dir` named `prefix` relative to the
/// root, is to the walk. Disk work.
fn classify(dir: &Path, prefix: &str, entry: io::Result<std::fs::DirEntry>) -> Kind {
    let Ok(entry) = entry else {
        return Kind::Counted(Visit::Unlisted);
    };
    let os_name = entry.file_name();
    let file_type = entry.file_type().ok();
    let name = os_name.to_str();
    if let Some(name) = name
        && is_ignored(name, file_type)
    {
        return Kind::Ignored(name.to_owned());
    }
    let (Some(name), Some(file_type)) = (name, file_type) else {
        return Kind::Counted(Visit::Unlisted);
    };
    Kind::Counted(place(dir.join(&os_name), prefix, name, file_type))
}

/// What the entry at `path`, named `name` in the folder named `prefix`
/// relative to the root, of the no-follow `file_type`, is to the walk.
/// Disk work.
fn place(path: PathBuf, prefix: &str, name: &str, file_type: FileType) -> Visit {
    if file_type.is_symlink() {
        return Visit::Unlisted;
    }
    let rel = if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    };
    if file_type.is_dir() {
        return Visit::Dir(path, rel);
    }
    if !file_type.is_file() {
        return Visit::Unlisted;
    }
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => Visit::File(ScannedFile {
            name: rel,
            path,
            size: meta.len(),
        }),
        _ => Visit::Unlisted,
    }
}

/// What a `StatPath` path names, and its size for a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathStat {
    pub kind: PathKind,
    pub size: u64,
}

impl PathStat {
    /// The answer for a missing or refused path.
    pub const MISSING: Self = Self {
        kind: PathKind::Missing,
        size: 0,
    };
}

/// What `rel` names under `root`, resolved as a folder fetch resolves it
/// ([`file_fetch::resolve_inside`]). A refused path, one resolving outside
/// the root included, reads as missing, so nothing outside the root is
/// revealed. Disk work: run it on a blocking thread.
#[must_use]
pub fn stat(root: &Path, rel: &str) -> PathStat {
    let Ok(target) = file_fetch::resolve_inside(root, rel) else {
        return PathStat::MISSING;
    };
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() => PathStat {
            kind: PathKind::Dir,
            size: 0,
        },
        Ok(meta) if meta.is_file() => PathStat {
            kind: PathKind::File,
            size: meta.len(),
        },
        Ok(_) => PathStat {
            kind: PathKind::Other,
            size: 0,
        },
        Err(_) => PathStat::MISSING,
    }
}

/// The pack's stream: a tar written into zstd written into chunks.
type Packer = tar::Builder<zstd::stream::write::Encoder<'static, ChunkWriter>>;

/// The error a cancelled read or write stops the pack with: never
/// `ErrorKind::Interrupted`, which `io::copy` and `write_all` retry.
fn cancelled() -> io::Error {
    io::Error::other("the folder fetch was cancelled")
}

/// The error a [`ChunkWriter`] that can send no more answers with.
fn stopped() -> io::Error {
    io::Error::other("the folder fetch stopped")
}

/// Send the files of `scan`, walked from `root`, over `tx` as `FileChunk`
/// frames (seq 0, 1, … with no gap) carrying one tar of them compressed
/// with zstd, and return the final message, unsent: `FileFetchDone` once
/// both the tar and the zstd stream are finished and their last bytes sent,
/// or a `FileFetchError` naming the file being packed. Once `cancel` fires
/// or `tx` closes, nothing further is sent and `None` is returned.
///
/// Only the scan's list is packed. Each file is opened again and checked
/// to be a regular file inside `root` ([`file_tail::open_inside`]); one
/// that is gone, is no longer a file, or does not open inside `root` is
/// skipped. Its header's size is the smaller of the scanned size and its
/// size at open, and exactly that many bytes follow: a file that grew is
/// cut, one that shrank fails the fetch. Every chunk but the last is
/// exactly [`CHUNK_SIZE`] bytes, and nothing is sent once the pack stops.
/// Disk work: run it on a blocking thread.
pub fn pack(
    id: &str,
    scan: &Scan,
    root: &CanonicalRoot,
    tx: &mpsc::Sender<DaemonMessage>,
    cancel: &CancellationToken,
) -> Option<DaemonMessage> {
    let writer = ChunkWriter::new(id, tx.clone(), cancel.clone());
    let encoder = match zstd::stream::write::Encoder::new(writer, 1) {
        Ok(encoder) => encoder,
        Err(err) => return stopped_with(id, None, &err, tx, cancel),
    };
    let mut packer: Packer = tar::Builder::new(encoder);
    let folders = Folders::of_root(root);
    for file in &scan.files {
        if cancel.is_cancelled() {
            packer.get_mut().get_mut().poison();
            return None;
        }
        if let Err(err) = append(&mut packer, &folders, file, cancel) {
            // `tar::Builder`'s drop finishes the archive: a poisoned writer
            // sends none of it.
            packer.get_mut().get_mut().poison();
            return stopped_with(id, Some(&file.name), &err, tx, cancel);
        }
    }
    let finished = packer
        .into_inner()
        .and_then(zstd::stream::write::Encoder::finish)
        .and_then(ChunkWriter::finish);
    match finished {
        Ok(()) => Some(DaemonMessage::FileFetchDone { id: id.to_owned() }),
        Err(err) => stopped_with(id, None, &err, tx, cancel),
    }
}

/// The final message of a pack that stopped on `err` while packing the file
/// `name` (`None` for the archive itself): nothing when it was cancelled or
/// its receiver is gone, else a `FileFetchError`.
fn stopped_with(
    id: &str,
    name: Option<&str>,
    err: &io::Error,
    tx: &mpsc::Sender<DaemonMessage>,
    cancel: &CancellationToken,
) -> Option<DaemonMessage> {
    if cancel.is_cancelled() || tx.is_closed() {
        return None;
    }
    let error = match name {
        Some(name) => format!("packing {name}: {err}"),
        None => format!("packing the folder: {err}"),
    };
    Some(DaemonMessage::FileFetchError {
        id: id.to_owned(),
        error,
    })
}

/// Append `file` to `packer`, or skip it when it no longer opens as a
/// regular file inside `folders`. Disk work.
///
/// # Errors
/// Writing the stream failed, or the file's read failed or came up short
/// after its header was written.
fn append(
    packer: &mut Packer,
    folders: &Folders,
    file: &ScannedFile,
    cancel: &CancellationToken,
) -> io::Result<()> {
    let opened = match file_tail::open_inside(folders, &file.path) {
        Ok(opened) => opened,
        Err(StepError::Missing) => {
            debug!(file = %file.name, "folder_pack: a listed file is gone; skipped");
            return Ok(());
        }
        Err(StepError::Failed(why)) => {
            debug!(file = %file.name, %why, "folder_pack: a listed file does not open inside the folder; skipped");
            return Ok(());
        }
    };
    let meta = match opened.metadata() {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => {
            debug!(file = %file.name, "folder_pack: a listed file is no longer a file; skipped");
            return Ok(());
        }
        Err(err) => {
            debug!(file = %file.name, error = %err, "folder_pack: a listed file's metadata does not read; skipped");
            return Ok(());
        }
    };
    let size = file.size.min(meta.len());
    #[cfg(test)]
    hooks::after_open(&file.path);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(mtime(&meta));
    let data = ExactLen::new(opened, size, &file.name, cancel);
    packer.append_data(&mut header, &file.name, data)
}

/// `meta`'s modification time in Unix seconds, 0 when it is unavailable.
fn mtime(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs())
}

/// A reader yielding exactly `left` bytes of `inner`: it stops there however
/// much more `inner` holds, and fails when `inner` ends sooner or `cancel`
/// fires.
struct ExactLen<'a, R> {
    inner: R,
    left: u64,
    name: &'a str,
    cancel: &'a CancellationToken,
}

impl<'a, R: Read> ExactLen<'a, R> {
    fn new(inner: R, size: u64, name: &'a str, cancel: &'a CancellationToken) -> Self {
        Self {
            inner,
            left: size,
            name,
            cancel,
        }
    }
}

impl<R: Read> Read for ExactLen<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(cancelled());
        }
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = usize::try_from(self.left).map_or(buf.len(), |left| left.min(buf.len()));
        let read = self.inner.read(&mut buf[..want])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{} shrank while it was being sent", self.name),
            ));
        }
        self.left = self
            .left
            .saturating_sub(u64::try_from(read).unwrap_or(u64::MAX));
        Ok(read)
    }
}

/// The pack's sink: cuts what it is written into `FileChunk` frames of
/// exactly [`CHUNK_SIZE`] bytes, sent as each fills, and sends the last,
/// shorter one only from [`ChunkWriter::finish`]. Once a send fails or it is
/// poisoned it sends nothing more; it never sends from a drop.
struct ChunkWriter {
    id: String,
    tx: mpsc::Sender<DaemonMessage>,
    cancel: CancellationToken,
    buf: Vec<u8>,
    seq: u64,
    poisoned: bool,
}

impl ChunkWriter {
    fn new(id: &str, tx: mpsc::Sender<DaemonMessage>, cancel: CancellationToken) -> Self {
        Self {
            id: id.to_owned(),
            tx,
            cancel,
            buf: Vec::with_capacity(CHUNK_SIZE),
            seq: 0,
            poisoned: false,
        }
    }

    /// Send nothing more.
    fn poison(&mut self) {
        self.poisoned = true;
    }

    /// Send the buffer as the next chunk, waiting for room unless the fetch
    /// is cancelled; poisons on failure.
    fn send_buffer(&mut self) -> io::Result<()> {
        let chunk = DaemonMessage::FileChunk {
            id: self.id.clone(),
            seq: self.seq,
            data_b64: base64::engine::general_purpose::STANDARD.encode(&self.buf),
        };
        if !file_fetch::send_unless_cancelled_blocking(&self.tx, &self.cancel, chunk) {
            self.poison();
            return Err(stopped());
        }
        self.seq += 1;
        self.buf.clear();
        Ok(())
    }

    /// Send what is left as the last chunk, then send nothing more.
    ///
    /// # Errors
    /// The writer was poisoned, or the send failed.
    fn finish(mut self) -> io::Result<()> {
        if self.poisoned {
            return Err(stopped());
        }
        if !self.buf.is_empty() {
            self.send_buffer()?;
        }
        self.poison();
        Ok(())
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.poisoned {
            return Err(stopped());
        }
        if self.cancel.is_cancelled() {
            self.poison();
            return Err(cancelled());
        }
        let taken = (CHUNK_SIZE - self.buf.len()).min(data.len());
        self.buf.extend_from_slice(&data[..taken]);
        if self.buf.len() == CHUNK_SIZE {
            self.send_buffer()?;
        }
        Ok(taken)
    }

    /// Sends nothing: every chunk but the last stays full.
    fn flush(&mut self) -> io::Result<()> {
        if self.poisoned {
            Err(stopped())
        } else {
            Ok(())
        }
    }
}

/// `len` bytes no compressor can shrink, the same for the same `seed`: an
/// xorshift stream, for tests that need a stream of many chunks.
#[cfg(test)]
pub(crate) fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Acting on a file the pack just opened, from a test: to change it after
/// its checks and before its header is written.
#[cfg(test)]
pub(crate) mod hooks {
    use crate::sync::lock;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    type Action = Box<dyn Fn(&Path) + Send>;

    static ACTIONS: Mutex<Vec<(u64, OsString, Action)>> = Mutex::new(Vec::new());

    /// Runs an action on every pack's open of a file of one name, until
    /// dropped.
    pub(crate) struct AfterOpen(u64);

    impl AfterOpen {
        /// Run `action` on each opened file named `name`, given its path.
        pub(crate) fn new(name: &str, action: impl Fn(&Path) + Send + 'static) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let key = NEXT.fetch_add(1, Ordering::Relaxed);
            lock(&ACTIONS).push((key, name.into(), Box::new(action)));
            Self(key)
        }
    }

    impl Drop for AfterOpen {
        fn drop(&mut self) {
            lock(&ACTIONS).retain(|(key, _, _)| *key != self.0);
        }
    }

    /// The pack opened `path` and checked it.
    pub(super) fn after_open(path: &Path) {
        for (_, name, action) in lock(&ACTIONS).iter() {
            if path.file_name() == Some(name.as_os_str()) {
                action(path);
            }
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::file_fetch::confine_dir;
    use crate::file_fetch::test_support::TestDir;
    use crate::file_tail::hooks::{make_dir_link, make_junction};
    use std::path::{Path, PathBuf};

    /// `base/root` as a fetch's repo root, beside `base/outside`, a folder
    /// holding `secret.txt`.
    fn root_with_outside(tag: &str) -> (TestDir, PathBuf, PathBuf) {
        let base = TestDir::new(tag);
        let root = base.path().join("root");
        std::fs::create_dir_all(&root).expect("create root");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&outside).expect("create outside");
        std::fs::write(outside.join("secret.txt"), "secret").expect("write secret");
        (base, root, outside)
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, contents).expect("write file");
    }

    fn scan_of(root: &Path, rel: &str, limits: Limits) -> Result<Scan, ScanError> {
        let dir = confine_dir(root, rel).expect("a folder inside the root");
        scan(&dir, limits, &CancellationToken::new())
    }

    fn scan_all(root: &Path) -> Scan {
        scan_of(root, "", Limits::FETCH).expect("the walk passes")
    }

    fn names(scan: &Scan) -> Vec<&str> {
        let mut names: Vec<&str> = scan.files.iter().map(|file| file.name.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// A symlink to the file `target`: whether one was made (Windows needs
    /// developer mode or elevation).
    fn make_file_link(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    #[test]
    fn links_in_the_tree_are_skipped_not_followed() {
        let (_base, root, outside) = root_with_outside("pack-links");
        write(&root.join("top.txt"), "top");
        write(&root.join("sub").join("real.txt"), "real");
        let mut links = 0;
        for (link, target) in [
            (root.join("dir-in"), root.join("sub")),
            (root.join("dir-out"), outside.clone()),
        ] {
            links += u64::from(make_dir_link(&link, &target));
        }
        for (link, target) in [
            (root.join("junction-in"), root.join("sub")),
            (root.join("junction-out"), outside.clone()),
        ] {
            links += u64::from(make_junction(&link, &target));
        }
        for (link, target) in [
            (root.join("file-in.txt"), root.join("top.txt")),
            (root.join("file-out.txt"), outside.join("secret.txt")),
        ] {
            links += u64::from(make_file_link(&link, &target));
        }
        let scan = scan_all(&root);
        assert_eq!(names(&scan), ["sub/real.txt", "top.txt"]);
        assert_eq!(scan.bytes, 7);
        assert_eq!(scan.unlisted, links, "every link is counted");
    }

    #[test]
    fn a_junction_loop_to_an_ancestor_ends_the_walk() {
        let (_base, root, _) = root_with_outside("pack-loop");
        write(&root.join("sub").join("f.txt"), "f");
        if !make_junction(&root.join("sub").join("loop"), &root) {
            return;
        }
        let scan = scan_all(&root);
        assert_eq!(names(&scan), ["sub/f.txt"]);
        assert_eq!(scan.unlisted, 1);
    }

    #[test]
    fn ignored_dirs_are_skipped_at_any_depth() {
        let (_base, root, _) = root_with_outside("pack-ignored");
        write(&root.join("keep.txt"), "k");
        write(&root.join("node_modules").join("a.js"), "a");
        write(&root.join(".git").join("HEAD"), "ref");
        write(&root.join("pkg").join("target").join("b.o"), "b");
        write(&root.join("pkg").join("deep").join(".venv").join("c"), "c");
        write(&root.join("pkg").join("src").join("lib.rs"), "lib");
        // A worktree's `.git` is a file, skipped all the same.
        write(&root.join("pkg").join(".git"), "gitdir: elsewhere");
        let scan = scan_all(&root);
        assert_eq!(names(&scan), ["keep.txt", "pkg/src/lib.rs"]);
        let mut skipped = scan.skipped.clone();
        skipped.sort_unstable();
        assert_eq!(skipped, [".git", ".venv", "node_modules", "target"]);
        assert_eq!(scan.unlisted, 0, "an ignored entry is no unlisted one");
    }

    #[test]
    fn ignored_names_match_in_any_ascii_case() {
        let (_base, root, _) = root_with_outside("pack-case");
        write(&root.join("keep.txt"), "k");
        write(&root.join("NODE_MODULES").join("a.js"), "a");
        write(&root.join("Target").join("b.o"), "b");
        write(&root.join("x").join(".VENV").join("c"), "c");
        write(&root.join("y").join(".Git").join("HEAD"), "ref");
        let scan = scan_all(&root);
        assert_eq!(names(&scan), ["keep.txt"]);
    }

    #[test]
    fn near_miss_names_are_kept() {
        let (_base, root, _) = root_with_outside("pack-near-miss");
        write(&root.join("targets").join("a"), "a");
        write(&root.join(".venv2").join("b"), "b");
        write(&root.join("my_target").join("c"), "c");
        write(&root.join("node_modules.txt"), "d");
        // A file named like an ignored folder is a file, kept.
        write(&root.join("sub").join("target"), "e");
        let scan = scan_all(&root);
        assert_eq!(
            names(&scan),
            [
                ".venv2/b",
                "my_target/c",
                "node_modules.txt",
                "sub/target",
                "targets/a"
            ]
        );
        assert!(scan.skipped.is_empty(), "{:?}", scan.skipped);
    }

    #[test]
    fn the_fetch_root_itself_is_never_ignored() {
        let (_base, root, _) = root_with_outside("pack-root-ignored");
        write(&root.join("target").join("doc").join("index.html"), "<p>");
        write(
            &root
                .join("target")
                .join("doc")
                .join("x")
                .join("target")
                .join("y"),
            "y",
        );
        let scan = scan_of(&root, "target/doc", Limits::FETCH).expect("the walk passes");
        assert_eq!(names(&scan), ["index.html"]);
        assert_eq!(scan.skipped, ["target"]);
        let scan = scan_of(&root, "target", Limits::FETCH).expect("the walk passes");
        assert_eq!(names(&scan), ["doc/index.html"]);
    }

    #[test]
    fn entry_names_are_relative_slash_separated_and_unprefixed() {
        let (_base, root, _) = root_with_outside("pack-names");
        write(&root.join("a").join("b").join("c.txt"), "c");
        let scan = scan_all(&root);
        let canonical = std::fs::canonicalize(&root).expect("canonical root");
        assert_eq!(scan.files.len(), 1);
        let file = &scan.files[0];
        assert_eq!(file.name, "a/b/c.txt");
        assert!(!file.name.contains('\\') && !file.name.contains('?'));
        assert!(!file.name.starts_with('/'));
        assert!(file.path.starts_with(&canonical), "{}", file.path.display());
        assert!(file.path.ends_with("a/b/c.txt") || file.path.ends_with(r"a\b\c.txt"));
        assert_eq!(file.size, 1);
    }

    #[test]
    fn a_name_that_is_not_unicode_is_skipped() {
        let (_base, root, _) = root_with_outside("pack-not-unicode");
        write(&root.join("ok.txt"), "ok");
        #[cfg(windows)]
        let name: std::ffi::OsString = {
            use std::os::windows::ffi::OsStringExt as _;
            // `a` and an unpaired surrogate.
            std::ffi::OsString::from_wide(&[0x61, 0xD800])
        };
        #[cfg(unix)]
        let name: std::ffi::OsString = {
            use std::os::unix::ffi::OsStrExt as _;
            std::ffi::OsStr::from_bytes(b"a\xff").to_os_string()
        };
        // Some file systems (APFS) refuse such a name: nothing to test there.
        if std::fs::write(root.join(&name), "bad").is_err() {
            return;
        }
        let scan = scan_all(&root);
        assert_eq!(names(&scan), ["ok.txt"]);
        assert_eq!(scan.unlisted, 1);
        std::fs::remove_file(root.join(&name)).expect("clean up");
    }

    fn small(files: u64, bytes: u64) -> Limits {
        Limits {
            files,
            bytes,
            visits: 1_000,
        }
    }

    #[test]
    fn refuses_past_the_file_ceiling_before_started() {
        let (_base, root, _) = root_with_outside("pack-file-ceiling");
        for n in 0..4 {
            write(&root.join(format!("f{n}")), "x");
        }
        let err = scan_of(&root, "", small(3, 1_000)).expect_err("past the file ceiling");
        assert!(matches!(err, ScanError::Files { limit: 3 }), "{err:?}");
        assert_eq!(err.to_string(), "the folder has more than 3 files");
        assert_eq!(
            ScanError::Files { limit: 20_000 }.to_string(),
            "the folder has more than 20000 files"
        );
    }

    #[test]
    fn refuses_past_the_byte_ceiling_before_started() {
        let (_base, root, _) = root_with_outside("pack-byte-ceiling");
        write(&root.join("a"), "123456");
        write(&root.join("b"), "123456");
        let err = scan_of(&root, "", small(100, 10)).expect_err("past the byte ceiling");
        assert!(matches!(err, ScanError::Bytes { limit: 10 }), "{err:?}");
        assert_eq!(err.to_string(), "the folder holds more than 10 bytes");
        assert_eq!(
            ScanError::Bytes {
                limit: Limits::FETCH.bytes
            }
            .to_string(),
            "the folder holds more than 512 MiB"
        );
    }

    #[test]
    fn a_folder_exactly_at_the_ceilings_is_fetched() {
        let (_base, root, _) = root_with_outside("pack-at-ceiling");
        write(&root.join("a"), "1234");
        write(&root.join("b"), "1234");
        write(&root.join("c").join("d"), "12");
        let scan = scan_of(&root, "", small(3, 10)).expect("exactly at the ceilings");
        assert_eq!(names(&scan), ["a", "b", "c/d"]);
        assert_eq!(scan.bytes, 10);
    }

    #[test]
    fn a_tree_of_empty_dirs_past_the_visit_ceiling_is_refused() {
        let (_base, root, _) = root_with_outside("pack-visits");
        for n in 0..6 {
            std::fs::create_dir_all(root.join(format!("d{n}"))).expect("create dir");
        }
        let limits = Limits {
            files: 100,
            bytes: 100,
            visits: 5,
        };
        let err = scan_of(&root, "", limits).expect_err("past the visit ceiling");
        assert!(matches!(err, ScanError::Visits { limit: 5 }), "{err:?}");
        assert!(err.to_string().contains("more than 5 entries"), "{err}");
        let limits = Limits {
            visits: 6,
            ..limits
        };
        assert!(
            scan_of(&root, "", limits).is_ok(),
            "exactly at the visit ceiling"
        );
    }

    #[test]
    fn a_file_held_open_for_writing_counts_its_real_size() {
        use std::io::Write as _;
        let (_base, root, _) = root_with_outside("pack-held-open");
        let path = root.join("growing.log");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(&[7u8; 1000]).expect("write");
        file.flush().expect("flush");
        let scan = scan_all(&root);
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].size, 1000);
        assert_eq!(scan.bytes, 1000);
        drop(file);
    }

    #[test]
    fn names_with_trailing_dots_or_spaces_pack_their_own_bytes() {
        let (_base, root, _) = root_with_outside("pack-trailing");
        let canonical = std::fs::canonicalize(&root).expect("canonical root");
        for (name, contents) in [("x", "1"), ("x.", "22"), ("x ", "333")] {
            std::fs::write(canonical.join(name), contents).expect("write");
        }
        let scan = scan_all(&root);
        let mut found: Vec<(&str, u64)> = scan
            .files
            .iter()
            .map(|file| (file.name.as_str(), file.size))
            .collect();
        found.sort_unstable();
        assert_eq!(found, [("x", 1), ("x ", 3), ("x.", 2)]);
        for file in &scan.files {
            assert_eq!(
                std::fs::symlink_metadata(&file.path)
                    .expect("its own entry")
                    .len(),
                file.size,
                "{}",
                file.path.display()
            );
        }
        for name in ["x", "x.", "x "] {
            std::fs::remove_file(canonical.join(name)).expect("clean up");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_short_name_never_bypasses_the_ignore_list() {
        use crate::file_fetch::test_support::short_name;
        let (_base, root, _) = root_with_outside("pack-short-ignored");
        write(&root.join("keep.txt"), "k");
        write(&root.join("node_modules").join("a.js"), "a");
        // ReFS and the Dev Drive make no short names.
        if short_name(&root.join("node_modules")).is_none() {
            return;
        }
        let scan = scan_all(&root);
        assert_eq!(names(&scan), ["keep.txt"]);
        assert_eq!(scan.skipped, ["node_modules"]);
    }

    #[test]
    fn a_path_longer_than_max_path_is_packed() {
        let (_base, root, _) = root_with_outside("pack-long-path");
        let canonical = std::fs::canonicalize(&root).expect("canonical root");
        let part = "d".repeat(60);
        let mut deep = canonical.clone();
        for _ in 0..5 {
            deep.push(&part);
        }
        std::fs::create_dir_all(&deep).expect("create a deep folder");
        std::fs::write(deep.join("f.txt"), "deep").expect("write deep file");
        assert!(deep.join("f.txt").as_os_str().len() > 260);
        let scan = scan_all(&root);
        let expected = format!("{part}/{part}/{part}/{part}/{part}/f.txt");
        assert_eq!(names(&scan), [expected.as_str()]);
        assert_eq!(scan.files[0].size, 4);
    }

    #[test]
    fn a_reserved_name_in_the_tree_never_opens_a_device() {
        let (_base, root, _) = root_with_outside("pack-reserved");
        let canonical = std::fs::canonicalize(&root).expect("canonical root");
        std::fs::write(canonical.join("CON"), "con!").expect("write CON");
        std::fs::write(canonical.join("nul.txt"), "nul").expect("write nul.txt");
        let scan = scan_all(&root);
        let mut found: Vec<(&str, u64)> = scan
            .files
            .iter()
            .map(|file| (file.name.as_str(), file.size))
            .collect();
        found.sort_unstable();
        assert_eq!(found, [("CON", 4), ("nul.txt", 3)]);
        // A request naming one is a file, refused promptly.
        let err = confine_dir(&root, "CON").expect_err("a file is no folder");
        assert!(format!("{err:#}").contains("is not a folder"), "{err:#}");
        assert_eq!(
            stat(&root, "CON"),
            PathStat {
                kind: PathKind::File,
                size: 4
            }
        );
        for name in ["CON", "nul.txt"] {
            std::fs::remove_file(canonical.join(name)).expect("clean up");
        }
    }

    #[test]
    fn stat_path_through_an_outside_link_reveals_nothing() {
        let (_base, root, outside) = root_with_outside("pack-stat");
        write(&root.join("a.txt"), "abc");
        std::fs::create_dir_all(root.join("sub")).expect("create sub");
        assert_eq!(
            stat(&root, "a.txt"),
            PathStat {
                kind: PathKind::File,
                size: 3
            }
        );
        for rel in ["", ".", "sub"] {
            assert_eq!(
                stat(&root, rel),
                PathStat {
                    kind: PathKind::Dir,
                    size: 0
                },
                "{rel:?}"
            );
        }
        let outside_abs = outside.join("secret.txt");
        let outside_abs = outside_abs.to_str().expect("utf-8 temp path");
        for rel in ["nope", "../outside/secret.txt", "..", outside_abs] {
            assert_eq!(stat(&root, rel), PathStat::MISSING, "{rel:?}");
        }
        if !make_dir_link(&root.join("link"), &outside) {
            return;
        }
        for rel in ["link", "link/secret.txt", "link/nope"] {
            assert_eq!(stat(&root, rel), PathStat::MISSING, "{rel:?}");
        }
    }

    /// The bound on every wait of these tests; passing it fails as hung.
    const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(120);

    /// One archive entry: its name, its type and its bytes.
    type Entry = (String, tar::EntryType, Vec<u8>);

    /// Run [`pack`] of `scan`, walked from `root`, on a thread of its own
    /// over a queue of `capacity`, draining the queue until the pack drops
    /// its sender: everything sent, and the final message.
    fn pack_all(
        root: &Path,
        scan: Scan,
        capacity: usize,
        cancel: &CancellationToken,
    ) -> (Vec<DaemonMessage>, Option<DaemonMessage>) {
        let dir = confine_dir(root, "").expect("the root is a folder");
        let (tx, mut rx) = mpsc::channel(capacity);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            let last = pack("f", &scan, &dir, &tx, &cancel);
            drop(tx);
            let _ = done_tx.send(last);
        });
        let (sent_tx, sent_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut sent = Vec::new();
            while let Some(msg) = rx.blocking_recv() {
                sent.push(msg);
            }
            let _ = sent_tx.send(sent);
        });
        let sent = sent_rx
            .recv_timeout(HANG_GUARD)
            .expect("hung: the pack never stopped sending");
        let last = done_rx
            .recv_timeout(HANG_GUARD)
            .expect("hung: the pack never returned");
        (sent, last)
    }

    fn pack_root(root: &Path) -> (Vec<DaemonMessage>, Option<DaemonMessage>) {
        pack_all(root, scan_all(root), 4, &CancellationToken::new())
    }

    /// Each chunk's decoded bytes, checking the chunks run seq 0, 1, … with
    /// no gap.
    fn chunk_bytes(sent: &[DaemonMessage]) -> Vec<Vec<u8>> {
        sent.iter()
            .enumerate()
            .map(|(n, msg)| match msg {
                DaemonMessage::FileChunk {
                    id, seq, data_b64, ..
                } => {
                    assert_eq!(id, "f");
                    assert_eq!(*seq, u64::try_from(n).expect("a small seq"), "a gap");
                    base64::engine::general_purpose::STANDARD
                        .decode(data_b64)
                        .expect("base64 chunk")
                }
                other => unreachable!("only chunks are sent, got {other:?}"),
            })
            .collect()
    }

    /// The archive the chunks carry, decompressed.
    fn archive_bytes(sent: &[DaemonMessage]) -> Vec<u8> {
        let compressed = chunk_bytes(sent).concat();
        let mut decoder =
            zstd::stream::read::Decoder::new(compressed.as_slice()).expect("open the decoder");
        let mut tar_bytes = Vec::new();
        decoder
            .read_to_end(&mut tar_bytes)
            .expect("a whole zstd stream");
        tar_bytes
    }

    /// The archive's entries as `tar::Archive::entries` reads them (a GNU
    /// long name folded into its entry's name), sorted by name.
    fn unpack(sent: &[DaemonMessage]) -> Vec<Entry> {
        let tar_bytes = archive_bytes(sent);
        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        let mut entries: Vec<Entry> = archive
            .entries()
            .expect("a tar")
            .map(|entry| {
                let mut entry = entry.expect("an entry");
                let name = entry
                    .path()
                    .expect("a name")
                    .to_string_lossy()
                    .replace('\\', "/");
                let kind = entry.header().entry_type();
                let mut data = Vec::new();
                entry.read_to_end(&mut data).expect("its bytes");
                (name, kind, data)
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    fn is_done(last: Option<&DaemonMessage>) -> bool {
        matches!(last, Some(DaemonMessage::FileFetchDone { id }) if id == "f")
    }

    fn entry(name: &str, data: &[u8]) -> Entry {
        (name.to_owned(), tar::EntryType::Regular, data.to_vec())
    }

    /// Whether any entry holds `needle`.
    fn holds_bytes(entries: &[Entry], needle: &[u8]) -> bool {
        entries
            .iter()
            .any(|(_, _, data)| data.windows(needle.len()).any(|window| window == needle))
    }

    #[test]
    fn folder_pack_streams_a_tar_zst_that_unpacks_to_the_tree() {
        let (_base, root, _) = root_with_outside("pack-stream");
        let big = noise(CHUNK_SIZE * 2 + 7, 1);
        write(&root.join("a.txt"), "abc");
        write(&root.join("sub").join("b.txt"), "b");
        std::fs::create_dir_all(root.join("sub").join("deep")).expect("create deep");
        std::fs::write(root.join("sub").join("deep").join("c.bin"), &big).expect("write c.bin");
        let (sent, last) = pack_root(&root);
        assert!(is_done(last.as_ref()), "{last:?}");
        assert!(sent.len() >= 3, "{} chunks", sent.len());
        assert_eq!(
            unpack(&sent),
            [
                entry("a.txt", b"abc"),
                entry("sub/b.txt", b"b"),
                entry("sub/deep/c.bin", &big),
            ]
        );
    }

    #[test]
    fn an_empty_folder_sends_a_valid_empty_archive() {
        let (_base, root, _) = root_with_outside("pack-empty");
        let (sent, last) = pack_root(&root);
        assert!(is_done(last.as_ref()), "{last:?}");
        assert!(!sent.is_empty(), "an empty archive is still a stream");
        assert_eq!(unpack(&sent), Vec::<Entry>::new());
    }

    #[test]
    fn chunks_are_full_256_kib_except_the_last() {
        let (_base, root, _) = root_with_outside("pack-full-chunks");
        std::fs::write(root.join("noise.bin"), noise(CHUNK_SIZE * 3 + 100, 2))
            .expect("write noise");
        let (sent, last) = pack_root(&root);
        assert!(is_done(last.as_ref()), "{last:?}");
        let chunks = chunk_bytes(&sent);
        assert!(chunks.len() >= 4, "{} chunks", chunks.len());
        let (tail, full) = chunks.split_last().expect("chunks");
        for chunk in full {
            assert_eq!(chunk.len(), CHUNK_SIZE);
        }
        assert!(
            !tail.is_empty() && tail.len() <= CHUNK_SIZE,
            "{}",
            tail.len()
        );
    }

    #[test]
    fn the_archive_has_only_regular_entries() {
        let (_base, root, _) = root_with_outside("pack-regular");
        let long = format!("{}.txt", "n".repeat(120));
        std::fs::create_dir_all(root.join("empty")).expect("create empty");
        write(&root.join("sub").join(&long), "long");
        write(&root.join("short.txt"), "s");
        let (sent, last) = pack_root(&root);
        assert!(is_done(last.as_ref()), "{last:?}");
        let tar_bytes = archive_bytes(&sent);
        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        let kinds: Vec<tar::EntryType> = archive
            .entries()
            .expect("a tar")
            .raw(true)
            .map(|entry| entry.expect("an entry").header().entry_type())
            .collect();
        let count = |wanted: tar::EntryType| kinds.iter().filter(|kind| **kind == wanted).count();
        assert_eq!(count(tar::EntryType::Regular), 2, "{kinds:?}");
        assert_eq!(count(tar::EntryType::GNULongName), 1, "{kinds:?}");
        assert!(
            kinds
                .iter()
                .all(|kind| matches!(kind, tar::EntryType::Regular | tar::EntryType::GNULongName)),
            "{kinds:?}"
        );
        assert_eq!(
            unpack(&sent),
            [
                entry("short.txt", b"s"),
                entry(&format!("sub/{long}"), b"long"),
            ]
        );
    }

    #[test]
    fn the_archive_never_exceeds_the_announced_counts() {
        let (_base, root, _) = root_with_outside("pack-counts");
        write(&root.join("grow.txt"), "abc");
        write(&root.join("gone.txt"), "x");
        write(&root.join("keep.txt"), "k");
        let scan = scan_all(&root);
        let (files, bytes) = (scan.files.len(), scan.bytes);
        // Between the walk and the pack: one file grows, one is added, one
        // is deleted.
        write(&root.join("grow.txt"), "abcdefgh");
        write(&root.join("new.txt"), "new");
        std::fs::remove_file(root.join("gone.txt")).expect("delete gone.txt");
        let (sent, last) = pack_all(&root, scan, 4, &CancellationToken::new());
        assert!(is_done(last.as_ref()), "{last:?}");
        let entries = unpack(&sent);
        assert_eq!(
            entries,
            [entry("grow.txt", b"abc"), entry("keep.txt", b"k")]
        );
        assert!(entries.len() <= files);
        let sent_bytes: usize = entries.iter().map(|(_, _, data)| data.len()).sum();
        assert!(u64::try_from(sent_bytes).expect("small") <= bytes);
    }

    #[test]
    fn exact_len_reader_fails_a_short_source_and_cuts_a_long_one() {
        let cancel = CancellationToken::new();
        let mut short = ExactLen::new(&[1u8; 10][..], 20, "short.bin", &cancel);
        let err = io::copy(&mut short, &mut io::sink()).expect_err("a short source");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(err.to_string(), "short.bin shrank while it was being sent");
        let mut long = ExactLen::new(&[2u8; 30][..], 20, "long.bin", &cancel);
        let mut out = Vec::new();
        long.read_to_end(&mut out).expect("a long source");
        assert_eq!(out, [2u8; 20]);
        cancel.cancel();
        let mut stopped = ExactLen::new(&[3u8; 30][..], 20, "c.bin", &cancel);
        let err = stopped.read(&mut [0u8; 8]).expect_err("cancelled");
        assert_ne!(err.kind(), io::ErrorKind::Interrupted);
    }

    /// A root holding `big.bin`, incompressible and several chunks long,
    /// which the pack's open of it shrinks to `shrink_to` bytes, after its
    /// checks and before its header.
    fn shrinking_big_bin(tag: &str, shrink_to: u64) -> (TestDir, PathBuf, hooks::AfterOpen) {
        let (base, root, _) = root_with_outside(tag);
        write(&root.join("a.txt"), "abc");
        std::fs::write(root.join("big.bin"), noise(CHUNK_SIZE * 3 + 1000, 3))
            .expect("write big.bin");
        let hook = hooks::AfterOpen::new("big.bin", move |path| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("open big.bin for writing")
                .set_len(shrink_to)
                .expect("shrink big.bin");
        });
        (base, root, hook)
    }

    #[test]
    fn a_file_that_shrinks_after_its_header_fails_the_fetch() {
        let (_base, root, _hook) = shrinking_big_bin("pack-shrinks", 10);
        let (_, last) = pack_root(&root);
        match last {
            Some(DaemonMessage::FileFetchError { id, error }) => {
                assert_eq!(id, "f");
                assert!(error.contains("big.bin"), "{error}");
                assert!(error.contains("shrank"), "{error}");
            }
            other => unreachable!("expected FileFetchError, got {other:?}"),
        }
    }

    #[test]
    fn no_chunk_follows_an_error_or_a_cancel() {
        // An error: big.bin shrinks after its header, part of it sent.
        let (_base, root, _hook) = shrinking_big_bin(
            "pack-no-chunk-after",
            u64::try_from(CHUNK_SIZE * 2 + 1).expect("small"),
        );
        let (sent, last) = pack_root(&root);
        assert!(
            matches!(last, Some(DaemonMessage::FileFetchError { .. })),
            "{last:?}"
        );
        assert!(
            !sent.is_empty(),
            "some of big.bin was sent before the error"
        );
        for chunk in chunk_bytes(&sent) {
            assert_eq!(
                chunk.len(),
                CHUNK_SIZE,
                "a partial chunk followed the error"
            );
        }
        // A cancel: big.bin's open cancels the fetch.
        let (_base, root, _) = root_with_outside("pack-no-chunk-after-cancel");
        std::fs::write(root.join("big.bin"), noise(CHUNK_SIZE * 3 + 1000, 4))
            .expect("write big.bin");
        let cancel = CancellationToken::new();
        let firing = cancel.clone();
        let _hook = hooks::AfterOpen::new("big.bin", move |_| firing.cancel());
        let (sent, last) = pack_all(&root, scan_all(&root), 4, &cancel);
        assert!(last.is_none(), "{last:?}");
        for chunk in chunk_bytes(&sent) {
            assert_eq!(
                chunk.len(),
                CHUNK_SIZE,
                "a partial chunk followed the cancel"
            );
        }
    }

    #[test]
    fn a_dir_swapped_for_a_junction_after_the_scan_packs_nothing_from_outside() {
        let (_base, root, outside) = root_with_outside("pack-swap-dir");
        write(&root.join("keep.txt"), "k");
        write(&root.join("sub").join("secret.txt"), "inside!");
        let scan = scan_all(&root);
        std::fs::remove_dir_all(root.join("sub")).expect("remove sub");
        if !make_dir_link(&root.join("sub"), &outside) {
            // No link could be made here: nothing to test.
            return;
        }
        let (sent, last) = pack_all(&root, scan, 4, &CancellationToken::new());
        assert!(is_done(last.as_ref()), "{last:?}");
        let entries = unpack(&sent);
        assert_eq!(entries, [entry("keep.txt", b"k")]);
        assert!(!holds_bytes(&entries, b"secret"));
    }

    #[test]
    fn a_file_swapped_for_a_symlink_after_the_scan_is_skipped() {
        let (_base, root, outside) = root_with_outside("pack-swap-file");
        write(&root.join("keep.txt"), "k");
        write(&root.join("f.txt"), "inside");
        let scan = scan_all(&root);
        std::fs::remove_file(root.join("f.txt")).expect("remove f.txt");
        if !make_file_link(&root.join("f.txt"), &outside.join("secret.txt")) {
            // No symlink could be made here (Windows without developer
            // mode): nothing to test.
            return;
        }
        let (sent, last) = pack_all(&root, scan, 4, &CancellationToken::new());
        assert!(is_done(last.as_ref()), "{last:?}");
        let entries = unpack(&sent);
        assert_eq!(entries, [entry("keep.txt", b"k")]);
        assert!(!holds_bytes(&entries, b"secret"));
    }

    #[test]
    fn a_file_swapped_for_a_directory_after_the_scan_is_skipped() {
        let (_base, root, _) = root_with_outside("pack-swap-to-dir");
        write(&root.join("keep.txt"), "k");
        write(&root.join("f.txt"), "file");
        let scan = scan_all(&root);
        std::fs::remove_file(root.join("f.txt")).expect("remove f.txt");
        write(&root.join("f.txt").join("inner.txt"), "inner");
        let (sent, last) = pack_all(&root, scan, 4, &CancellationToken::new());
        assert!(is_done(last.as_ref()), "{last:?}");
        assert_eq!(unpack(&sent), [entry("keep.txt", b"k")]);
    }

    #[cfg(windows)]
    #[test]
    fn an_unopenable_file_is_skipped_and_the_fetch_completes() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let (_base, root, _) = root_with_outside("pack-unopenable");
        write(&root.join("keep.txt"), "k");
        write(&root.join("locked.txt"), "locked");
        let scan = scan_all(&root);
        assert_eq!(names(&scan), ["keep.txt", "locked.txt"]);
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(root.join("locked.txt"))
            .expect("hold locked.txt with no sharing");
        let (sent, last) = pack_all(&root, scan, 4, &CancellationToken::new());
        drop(held);
        assert!(is_done(last.as_ref()), "{last:?}");
        assert_eq!(unpack(&sent), [entry("keep.txt", b"k")]);
    }

    #[test]
    fn a_hard_link_is_packed_as_a_regular_file() {
        let (_base, root, _) = root_with_outside("pack-hard-link");
        write(&root.join("a.txt"), "shared");
        std::fs::hard_link(root.join("a.txt"), root.join("b.txt")).expect("make a hard link");
        let (sent, last) = pack_root(&root);
        assert!(is_done(last.as_ref()), "{last:?}");
        assert_eq!(
            unpack(&sent),
            [entry("a.txt", b"shared"), entry("b.txt", b"shared")]
        );
    }

    #[cfg(windows)]
    #[test]
    fn alternate_data_streams_are_never_packed() {
        let (_base, root, _) = root_with_outside("pack-ads");
        write(&root.join("f.txt"), "main");
        std::fs::write(root.join("f.txt:hidden"), "hidden stream").expect("write a stream");
        let (sent, last) = pack_root(&root);
        assert!(is_done(last.as_ref()), "{last:?}");
        let entries = unpack(&sent);
        assert_eq!(entries, [entry("f.txt", b"main")]);
        assert!(!holds_bytes(&entries, b"hidden"));
    }

    /// Start [`pack`] of `root`'s incompressible multi-chunk file over a
    /// queue of one, read its first message, then run `then` on the
    /// receiver and the token: the pack's final message, once it returns.
    /// What `then` hands back is kept until then.
    fn pack_parked(
        tag: &str,
        then: impl FnOnce(
            mpsc::Receiver<DaemonMessage>,
            &CancellationToken,
        ) -> Option<mpsc::Receiver<DaemonMessage>>,
    ) -> Option<DaemonMessage> {
        let (_base, root, _) = root_with_outside(tag);
        std::fs::write(root.join("noise.bin"), noise(CHUNK_SIZE * 8, 5)).expect("write noise");
        let scan = scan_all(&root);
        let dir = confine_dir(&root, "").expect("the root is a folder");
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let packing = cancel.clone();
        std::thread::spawn(move || {
            let _ = done_tx.send(pack("f", &scan, &dir, &tx, &packing));
        });
        let (first_tx, first_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let first = rx.blocking_recv();
            let _ = first_tx.send(first);
            rx
        });
        let first = first_rx
            .recv_timeout(HANG_GUARD)
            .expect("hung: the pack sent nothing");
        assert!(
            matches!(first, Some(DaemonMessage::FileChunk { seq: 0, .. })),
            "{first:?}"
        );
        let rx = reader.join().expect("the reader thread");
        let _kept = then(rx, &cancel);
        done_rx
            .recv_timeout(HANG_GUARD)
            .expect("hung: the pack never returned")
    }

    #[test]
    fn cancel_unblocks_a_worker_parked_on_a_full_channel() {
        let last = pack_parked("pack-cancel-parked", |rx, cancel| {
            cancel.cancel();
            // The receiver stays open: the cancel alone frees the pack.
            Some(rx)
        });
        assert!(last.is_none(), "{last:?}");
    }

    #[test]
    fn a_dropped_receiver_ends_the_worker() {
        let last = pack_parked("pack-dropped-receiver", |rx, _| {
            drop(rx);
            None
        });
        assert!(last.is_none(), "{last:?}");
    }
}
