//! The folder fetch's walk: which files of a confined folder
//! ([`crate::file_fetch::confine_dir`]) a `FetchFolder` sends, within its
//! [`Limits`], and what a `StatPath` path names ([`stat`]).
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

use crate::file_fetch::{self, CanonicalRoot};
use protocol::PathKind;
use std::fs::FileType;
use std::io;
use std::path::{Path, PathBuf};
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
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "nothing reads the scan's file list yet")
)]
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
}
