//! The index of the files and folders the phone has fetched from a host: the
//! host, the host path, the copy's size and times, so a copy can be opened
//! again, refreshed when the host's file changed, and dropped once it went
//! unopened for too long.
//!
//! Every time here is milliseconds since the Unix epoch and passed in by the
//! caller: nothing in this module reads a clock.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "used by the phone fetch seam in the next part")
)]

use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How long a copy is kept after it was last opened: 30 days, in
/// milliseconds.
pub const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// One copy of a host file or folder kept on the phone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The host the copy came from, as the copies folder keys it.
    #[serde(default)]
    pub host: String,
    /// The path on the host the copy was fetched from.
    #[serde(default)]
    pub host_path: String,
    /// The copy's path relative to the copies folder.
    #[serde(default)]
    pub local_path: String,
    /// The host file's size in bytes when it was fetched.
    #[serde(default)]
    pub size: u64,
    /// The host file's modified time, when the host sent one.
    #[serde(default)]
    pub modified: Option<i64>,
    /// When the copy was fetched.
    #[serde(default)]
    pub fetched_at: i64,
    /// When the copy was last opened.
    #[serde(default)]
    pub last_opened: i64,
}

/// The copies the phone keeps, as `fetched.json` holds them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// The copies, in the order they were fetched.
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// How a copy compares to the host's file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// The host's file is the one the copy holds.
    Fresh,
    /// The host's file changed since the copy was fetched.
    Changed,
    /// The host gave no answer, so the copy may be out of date.
    Unknown,
}

/// What the host answered when asked about its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostStat {
    /// The host file's size in bytes.
    pub size: u64,
    /// The host file's modified time, when the host sent one.
    pub modified: Option<i64>,
}

impl Index {
    /// Reads the index at `path`.
    ///
    /// A missing file is an empty index. One that cannot be read or parsed is
    /// logged and read as empty, and is left as it is: it is only ever
    /// replaced by the next write.
    #[must_use]
    pub fn read(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "reading the fetched index");
                return Self::default();
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(index) => index,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "reading the fetched index");
                Self::default()
            }
        }
    }

    /// Writes the index to `path`, replacing it in one step: the bytes land in
    /// a temporary file beside it and the rename swaps it in.
    ///
    /// # Errors
    ///
    /// Fails when the index cannot be serialized, its folder cannot be made,
    /// or either the temporary file or the rename cannot be written.
    pub fn write_atomic(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = temp_path(path)?;
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Records `entry`, replacing an earlier copy of the same host path.
    pub fn upsert(&mut self, entry: Entry) {
        match self
            .entries
            .iter_mut()
            .find(|kept| kept.host == entry.host && kept.host_path == entry.host_path)
        {
            Some(kept) => *kept = entry,
            None => self.entries.push(entry),
        }
    }

    /// Marks the copy at `host` and `host_path` as opened at `now`; when no
    /// entry is there, nothing changes.
    #[cfg_attr(
        test,
        expect(dead_code, reason = "used by the fetch seam and the storage page next")
    )]
    pub fn touch_opened(&mut self, host: &str, host_path: &str, now: i64) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.host == host && entry.host_path == host_path)
        {
            entry.last_opened = now;
        }
    }

    /// The copies not opened for [`RETENTION_MS`] before `now`.
    #[must_use]
    pub fn expired(&self, now: i64) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| now.saturating_sub(entry.last_opened) >= RETENTION_MS)
            .collect()
    }

    /// The bytes every copy in the index takes.
    #[must_use]
    #[cfg_attr(
        test,
        expect(dead_code, reason = "used by the storage page in the next part")
    )]
    pub fn total_size(&self) -> u64 {
        self.entries
            .iter()
            .fold(0u64, |total, entry| total.saturating_add(entry.size))
    }

    /// Removes every copy under `copies_root` and empties the index.
    ///
    /// An entry whose relative path leaves `copies_root` is dropped without a
    /// delete. An entry behind a link or junction in a folder below
    /// `copies_root` is refused without a delete too, and stays. An entry
    /// whose copy is already gone leaves the index; one whose delete failed
    /// stays, so a later clear or sweep tries it again, and the first such
    /// error is returned.
    ///
    /// # Errors
    ///
    /// Fails with the first delete that failed for a reason other than the
    /// copy being gone.
    pub fn clear(&mut self, copies_root: &Path) -> io::Result<()> {
        self.clear_using(copies_root, remove_path)
    }

    fn clear_using(
        &mut self,
        copies_root: &Path,
        remove: impl Fn(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut first_error: Option<io::Error> = None;
        let mut kept = Vec::with_capacity(self.entries.len());
        for entry in std::mem::take(&mut self.entries) {
            let Some(path) = copy_path(copies_root, &entry) else {
                tracing::warn!(
                    host = %entry.host,
                    path = %entry.host_path,
                    "dropping a fetched entry whose copy path leaves the copies folder"
                );
                continue;
            };
            if let Some(link) = linked_folder(copies_root, Path::new(&entry.local_path)) {
                tracing::warn!(
                    path = %link.display(),
                    "refusing to delete a fetched copy through a linked folder"
                );
                if first_error.is_none() {
                    first_error = Some(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "{} is a link, so nothing under it is deleted",
                            link.display()
                        ),
                    ));
                }
                kept.push(entry);
                continue;
            }
            match remove(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "removing a fetched copy");
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                    kept.push(entry);
                }
            }
        }
        self.entries = kept;
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

/// How `entry` compares to the host's answer about its file: `answer` is
/// `None` when the host gave none.
///
/// A copy or an answer without a modified time is compared by size alone, so
/// only two present times that differ can make a same-sized copy `Changed`.
#[must_use]
pub fn freshness(entry: &Entry, answer: Option<HostStat>) -> Freshness {
    let Some(answer) = answer else {
        return Freshness::Unknown;
    };
    let size_matches = answer.size == entry.size;
    let fresh = match (entry.modified, answer.modified) {
        (Some(stored), Some(host)) => size_matches && stored == host,
        _ => size_matches,
    };
    if fresh {
        Freshness::Fresh
    } else {
        Freshness::Changed
    }
}

/// The copy's path under `root`, or `None` when its relative path could leave
/// `root`.
fn copy_path(root: &Path, entry: &Entry) -> Option<PathBuf> {
    let relative = Path::new(&entry.local_path);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(relative))
}

/// The first link or junction among the folders `relative` names below `root`,
/// leaving the copy itself out.
fn linked_folder(root: &Path, relative: &Path) -> Option<PathBuf> {
    let folders = relative.components().count().saturating_sub(1);
    let mut path = root.to_path_buf();
    for part in relative.components().take(folders) {
        path.push(part.as_os_str());
        if is_link(&path) {
            return Some(path);
        }
    }
    None
}

/// Whether `path` is a link or a junction rather than a plain folder.
fn is_link(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The temporary file [`Index::write_atomic`] writes before the rename.
fn temp_path(path: &Path) -> io::Result<PathBuf> {
    let Some(name) = path.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the index path names no file",
        ));
    };
    let mut tmp = name.to_os_string();
    tmp.push(".tmp");
    Ok(path.with_file_name(tmp))
}

/// Removes one copy: a folder whole, a file, and a path that is already gone
/// without an error.
fn remove_path(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rt-native-fetched-{}-{tag}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }

        fn index_path(&self) -> PathBuf {
            self.0.join("fetched.json")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry(local_path: &str) -> Entry {
        Entry {
            host: "host-a".to_owned(),
            host_path: "/abs/file.txt".to_owned(),
            local_path: local_path.to_owned(),
            size: 12,
            modified: Some(1_700_000_000_000),
            fetched_at: 1_700_000_000_000,
            last_opened: 1_700_000_000_000,
        }
    }

    #[test]
    fn a_missing_index_reads_empty() {
        let dir = TestDir::new("missing");
        assert!(
            Index::read(&dir.index_path()).entries.is_empty(),
            "a missing file reads as an empty index"
        );
        assert!(
            !dir.index_path().exists(),
            "reading an index never creates the file"
        );
    }

    #[test]
    fn a_malformed_index_reads_empty_and_is_kept() {
        let dir = TestDir::new("malformed");
        let path = dir.index_path();
        std::fs::write(&path, "{ not json").expect("write the malformed index");

        assert!(
            Index::read(&path).entries.is_empty(),
            "a malformed file reads as an empty index"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read the index back"),
            "{ not json",
            "a read leaves the file as it was"
        );
    }

    #[test]
    fn an_index_round_trips() {
        let dir = TestDir::new("round-trip");
        let mut index = Index::default();
        index.upsert(entry("host-a/abs/file.txt"));

        index
            .write_atomic(&dir.index_path())
            .expect("write the index");

        assert_eq!(Index::read(&dir.index_path()), index);
    }

    #[test]
    fn an_index_without_newer_fields_reads() {
        let dir = TestDir::new("older-fields");
        let path = dir.index_path();
        std::fs::write(
            &path,
            r#"{"entries":[{"host":"host-a","host_path":"/abs/file.txt"}]}"#,
        )
        .expect("write an older index");

        let index = Index::read(&path);

        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].size, 0);
        assert_eq!(index.entries[0].modified, None);
        assert_eq!(index.entries[0].fetched_at, 0);
        assert_eq!(index.entries[0].last_opened, 0);
    }

    #[test]
    fn upsert_replaces_the_same_host_path() {
        let mut index = Index::default();
        index.upsert(entry("host-a/abs/file.txt"));
        let mut changed = entry("host-a/abs/file.txt");
        changed.size = 99;

        index.upsert(changed);

        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].size, 99);
    }

    #[test]
    fn a_copy_unopened_for_thirty_days_is_expired() {
        let opened = 1_700_000_000_000;
        let mut index = Index::default();
        let mut copy = entry("host-a/abs/file.txt");
        copy.last_opened = opened;
        index.upsert(copy);

        assert!(
            index.expired(opened + 30 * DAY_MS - 60_000).is_empty(),
            "29 days, 23 hours and 59 minutes is kept"
        );
        assert_eq!(
            index.expired(opened + 30 * DAY_MS).len(),
            1,
            "30 days expires"
        );
    }

    #[test]
    fn freshness_is_changed_when_size_or_modified_differs() {
        let copy = entry("host-a/abs/file.txt");
        let same = HostStat {
            size: copy.size,
            modified: copy.modified,
        };
        assert_eq!(freshness(&copy, Some(same)), Freshness::Fresh);
        assert_eq!(
            freshness(
                &copy,
                Some(HostStat {
                    size: copy.size + 1,
                    modified: copy.modified,
                })
            ),
            Freshness::Changed,
            "a different size"
        );
        assert_eq!(
            freshness(
                &copy,
                Some(HostStat {
                    size: copy.size,
                    modified: Some(1),
                })
            ),
            Freshness::Changed,
            "a different modified time"
        );
        let no_stored_time = Entry {
            modified: None,
            ..copy
        };
        assert_eq!(
            freshness(
                &no_stored_time,
                Some(HostStat {
                    size: no_stored_time.size,
                    modified: Some(5),
                })
            ),
            Freshness::Fresh,
            "a host time against an entry without one compares size alone"
        );
        assert_eq!(
            freshness(
                &no_stored_time,
                Some(HostStat {
                    size: no_stored_time.size + 1,
                    modified: Some(5),
                })
            ),
            Freshness::Changed,
            "a different size, with only one modified time"
        );
    }

    #[test]
    fn freshness_compares_size_alone_without_a_modified_time() {
        let copy = entry("host-a/abs/file.txt");
        assert_eq!(
            freshness(
                &copy,
                Some(HostStat {
                    size: copy.size,
                    modified: None,
                })
            ),
            Freshness::Fresh
        );
        assert_eq!(
            freshness(
                &copy,
                Some(HostStat {
                    size: copy.size + 1,
                    modified: None,
                })
            ),
            Freshness::Changed
        );
    }

    #[test]
    fn freshness_is_unknown_without_an_answer() {
        assert_eq!(
            freshness(&entry("host-a/abs/file.txt"), None),
            Freshness::Unknown
        );
    }

    #[test]
    fn clear_removes_copies_and_empties_the_index() {
        let dir = TestDir::new("clear");
        let root = dir.0.join("copies");
        std::fs::create_dir_all(root.join("host-a/abs/folder")).expect("create a folder copy");
        std::fs::write(root.join("host-a/abs/file.txt"), b"hi").expect("write a copy");
        std::fs::write(root.join("host-a/abs/folder/inner.txt"), b"hi")
            .expect("write a nested copy");
        let mut index = Index::default();
        index.upsert(entry("host-a/abs/file.txt"));
        let mut folder = entry("host-a/abs/folder");
        folder.host_path = "/abs/folder".to_owned();
        index.upsert(folder);

        index.clear(&root).expect("clear the copies");

        assert!(index.entries.is_empty(), "every copy leaves the index");
        assert!(!root.join("host-a/abs/file.txt").exists());
        assert!(!root.join("host-a/abs/folder").exists());
    }

    #[test]
    fn clear_never_deletes_outside_the_copies_folder() {
        let dir = TestDir::new("escape");
        let root = dir.0.join("copies");
        std::fs::create_dir_all(&root).expect("create the copies folder");
        let outside = dir.0.join("outside.txt");
        std::fs::write(&outside, b"keep me").expect("write the outside file");
        let mut index = Index::default();
        for escaping in [
            "../outside.txt",
            "host-a/../../outside.txt",
            "/etc/passwd",
            "C:/Windows/win.ini",
        ] {
            let mut copy = entry(escaping);
            copy.host_path = escaping.to_owned();
            index.upsert(copy);
        }

        index.clear(&root).expect("clear the copies");

        assert!(index.entries.is_empty(), "escaping entries are dropped");
        assert!(outside.exists(), "a file outside the copies folder is kept");
    }

    #[test]
    fn clear_keeps_an_entry_it_could_not_delete() {
        let dir = TestDir::new("clear-failure");
        let root = dir.0.join("copies");
        std::fs::create_dir_all(root.join("host-a")).expect("create the copies folder");
        std::fs::write(root.join("host-a/file.txt"), b"hi").expect("write a copy");
        std::fs::write(root.join("host-a/stuck.txt"), b"hi").expect("write the stuck copy");
        let mut index = Index::default();
        index.upsert(entry("host-a/file.txt"));
        let mut stuck = entry("host-a/stuck.txt");
        stuck.host_path = "/abs/stuck.txt".to_owned();
        index.upsert(stuck);
        let stuck_path = root.join("host-a/stuck.txt");

        let error = index
            .clear_using(&root, |path| {
                if path == stuck_path {
                    return Err(io::Error::other("the file is busy"));
                }
                remove_path(path)
            })
            .expect_err("the stuck copy's delete fails");

        assert_eq!(error.to_string(), "the file is busy");
        assert_eq!(
            index.entries.len(),
            1,
            "the entry it could not delete stays"
        );
        assert_eq!(index.entries[0].local_path, "host-a/stuck.txt");
        assert!(
            !root.join("host-a/file.txt").exists(),
            "the copy it could delete is gone"
        );
    }

    /// A folder link inside the copies root: a junction on Windows, a symlink
    /// elsewhere.
    #[cfg(windows)]
    fn link_folder(target: &Path, link: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .expect("run mklink");
        assert!(status.success(), "mklink /J failed");
    }

    #[cfg(not(windows))]
    fn link_folder(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("create the folder link");
    }

    #[test]
    fn clear_never_deletes_through_a_folder_link() {
        let dir = TestDir::new("folder-link");
        let root = dir.0.join("copies");
        let outside = dir.0.join("outside");
        std::fs::create_dir_all(&outside).expect("create the outside folder");
        let outside_file = outside.join("keep.txt");
        std::fs::write(&outside_file, b"keep me").expect("write the outside file");
        std::fs::create_dir_all(&root).expect("create the copies folder");
        link_folder(&outside, &root.join("host-a"));
        let mut index = Index::default();
        index.upsert(entry("host-a/keep.txt"));

        let error = index
            .clear(&root)
            .expect_err("a linked folder below the root is refused");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(outside_file.exists(), "the file outside the root survives");
        assert_eq!(index.entries.len(), 1, "the entry it refused stays");
    }
}
