//! This daemon's stable id: a UUID kept in its `daemon-id` file (see
//! [`crate::paths::resolve_daemon_id_file`]) and sent in every `Welcome`, so a
//! client can tell connections to one daemon apart from connections to two.
//!
//! The file holds the bare UUID text with no newline. The first start writes
//! it; a missing, empty, non-UTF-8 or unparseable file is replaced with a
//! fresh id.

use std::path::Path;

use anyhow::Context as _;
use uuid::Uuid;

use crate::paths::{Dirs, resolve_daemon_id_file};

/// This daemon's id, from the file [`resolve_daemon_id_file`] names for
/// `dirs` ([`load_or_create`]).
///
/// # Errors
///
/// The file's place could not be resolved, or [`load_or_create`] failed.
pub fn load(dirs: &Dirs) -> anyhow::Result<String> {
    load_or_create(&resolve_daemon_id_file(dirs)?)
}

/// Read this daemon's id from `path`, minting and writing a new one when the
/// file is missing, empty, not UTF-8 or not a UUID; the file's folder is
/// created when missing.
///
/// # Errors
///
/// The file exists but could not be read, or a new id could not be written.
pub fn load_or_create(path: &Path) -> anyhow::Result<String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = std::str::from_utf8(&bytes).map(str::trim);
            if let Ok(text) = text
                && Uuid::parse_str(text).is_ok()
            {
                return Ok(text.to_owned());
            }
            tracing::warn!(
                path = %path.display(),
                "the daemon id file is empty or not a UUID; replacing it with a new id"
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "no daemon id yet; creating one");
        }
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", path.display()));
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let id = Uuid::new_v4().to_string();
    crate::atomic_file::write_atomic(path, id.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use std::path::PathBuf;

    use super::load_or_create;
    use uuid::Uuid;

    /// A fresh scratch folder for `tag` and the id file inside it.
    fn scratch_file(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("rt-daemon-id-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        let file = root.join("daemon-id");
        (root, file)
    }

    #[test]
    fn creates_a_uuid_once_and_reuses_it() {
        let (root, file) = scratch_file("reuse");
        let first = load_or_create(&file).expect("the first start creates an id");
        Uuid::parse_str(&first).expect("the id is a UUID");
        let on_disk = std::fs::read_to_string(&file).expect("the id is written");
        assert_eq!(on_disk, first, "the file holds the bare id, no newline");

        let second = load_or_create(&file).expect("a later start reads the id");
        assert_eq!(second, first);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn creates_the_files_folder_when_missing() {
        let (root, _) = scratch_file("missing-dir");
        let file = root.join("data").join("daemon-id");
        let id = load_or_create(&file).expect("the folder is created");
        assert_eq!(
            std::fs::read_to_string(&file).expect("the id is written"),
            id
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replaces_an_invalid_file() {
        for (tag, content) in [("garbage", "not a uuid\n"), ("empty", "")] {
            let (root, file) = scratch_file(tag);
            std::fs::write(&file, content).expect("write a bad id file");
            let id = load_or_create(&file).expect("a bad file is replaced");
            Uuid::parse_str(&id).expect("the new id is a UUID");
            let on_disk = std::fs::read_to_string(&file).expect("the new id is written");
            assert_eq!(on_disk, id, "{tag}: the new id replaces the file");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn replaces_a_file_that_is_not_utf8() {
        let (root, file) = scratch_file("not-utf8");
        std::fs::write(&file, [0xFF, 0xFE, 0x31, 0x00]).expect("write a UTF-16LE id file");
        let id = load_or_create(&file).expect("a file that is not UTF-8 is replaced");
        Uuid::parse_str(&id).expect("the new id is a UUID");
        let on_disk = std::fs::read_to_string(&file).expect("the new id is written");
        assert_eq!(on_disk, id, "the new id replaces the file");
        let _ = std::fs::remove_dir_all(&root);
    }
}
