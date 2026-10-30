//! This daemon's stable id: a UUID kept in `<config>/daemon-id` and sent in
//! every `Welcome`, so a client can tell connections to one daemon apart from
//! connections to two.
//!
//! The file holds the bare UUID text with no newline. The first start writes
//! it; a missing, empty, non-UTF-8 or unparseable file is replaced with a
//! fresh id.

use crate::paths::{Dirs, daemon_id_file};
use anyhow::Context as _;
use uuid::Uuid;

/// Read this daemon's id from `<config>/daemon-id`, minting and writing a new
/// one when the file is missing, empty, not UTF-8 or not a UUID.
///
/// # Errors
///
/// The file exists but could not be read, or a new id could not be written.
pub fn load_or_create(dirs: &Dirs) -> anyhow::Result<String> {
    let path = daemon_id_file(dirs);
    match std::fs::read(&path) {
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
    let id = Uuid::new_v4().to_string();
    crate::atomic_file::write_atomic(&path, id.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::load_or_create;
    use crate::paths::{Dirs, daemon_id_file};
    use uuid::Uuid;

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-daemon-id-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            remote_config_file: root.join("remote.json"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
        }
    }

    #[test]
    fn creates_a_uuid_once_and_reuses_it() {
        let dirs = scratch_dirs("reuse");
        let first = load_or_create(&dirs).expect("the first start creates an id");
        Uuid::parse_str(&first).expect("the id is a UUID");
        let on_disk = std::fs::read_to_string(daemon_id_file(&dirs)).expect("the id is written");
        assert_eq!(on_disk, first, "the file holds the bare id, no newline");

        let second = load_or_create(&dirs).expect("a later start reads the id");
        assert_eq!(second, first);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn replaces_an_invalid_file() {
        for (tag, content) in [("garbage", "not a uuid\n"), ("empty", "")] {
            let dirs = scratch_dirs(tag);
            std::fs::write(daemon_id_file(&dirs), content).expect("write a bad id file");
            let id = load_or_create(&dirs).expect("a bad file is replaced");
            Uuid::parse_str(&id).expect("the new id is a UUID");
            let on_disk =
                std::fs::read_to_string(daemon_id_file(&dirs)).expect("the new id is written");
            assert_eq!(on_disk, id, "{tag}: the new id replaces the file");
            let _ = std::fs::remove_dir_all(&dirs.config);
        }
    }

    #[test]
    fn replaces_a_file_that_is_not_utf8() {
        let dirs = scratch_dirs("not-utf8");
        std::fs::write(daemon_id_file(&dirs), [0xFF, 0xFE, 0x31, 0x00])
            .expect("write a UTF-16LE id file");
        let id = load_or_create(&dirs).expect("a file that is not UTF-8 is replaced");
        Uuid::parse_str(&id).expect("the new id is a UUID");
        let on_disk =
            std::fs::read_to_string(daemon_id_file(&dirs)).expect("the new id is written");
        assert_eq!(on_disk, id, "the new id replaces the file");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
