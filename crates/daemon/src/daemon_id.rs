//! This daemon's stable id: a UUID kept in its `daemon-id` file (see
//! [`crate::paths::resolve_daemon_id_file`]) and sent in every `Welcome`, so a
//! client can tell connections to one daemon apart from connections to two.
//!
//! The file holds the bare UUID text with no newline. The first start writes
//! it; a missing, empty, non-UTF-8 or unparseable file is replaced with a
//! fresh id.
//!
//! An id file copied to another machine — a profile restored or copied from
//! another one, an image built with the file — would make two machines report
//! one id, and a client hides one of them as a duplicate. So the id's sibling
//! file `daemon-id.machine` holds a digest of the identifier of the machine
//! that minted the id ([`stamp_digest`]), and the id is minted again when the
//! digest names another machine:
//!
//! - an id with no stamp beside it is kept, and the stamp is written: every id
//!   an earlier release wrote has no stamp, so an update keeps the id;
//! - an id stamped by this machine is kept;
//! - an id stamped by another machine is replaced, and the new id and this
//!   machine's stamp are written;
//! - a missing or invalid id is replaced as it always was, and the stamp is
//!   written beside it;
//! - when the machine's identifier cannot be read, the id is kept or replaced
//!   exactly as before the stamp existed, with no stamp read or written.
//!
//! The stamp catches an id file that moved to a machine with a different
//! identifier; it cannot catch a disk image cloned without a new
//! `MachineGuid`, whose clone reports the original's identifier.
//!
//! A stamp that cannot be read or written warns and never stops the daemon: a
//! valid id is returned unchecked, and a minted id keeps its id file without a
//! stamp, so the next start stamps it. The id file's own read and write errors
//! stay fatal, and the id is written before the stamp.

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

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
    load_or_create_on(path, machine_identifier().as_deref())
}

/// [`load_or_create`] over a caller-supplied machine identifier, so tests never
/// read the real one. The rule the module doc spells out: a valid id is kept
/// when it carries no stamp or this machine's stamp, and replaced when the
/// stamp names another machine; a missing or invalid id is replaced; with no
/// identifier, the id is handled as it always was and no stamp is read or
/// written.
///
/// # Errors
///
/// The id file exists but could not be read, or a new id could not be written.
/// A stamp failure is logged and never returned: it leaves the id in place.
fn load_or_create_on(path: &Path, machine: Option<&str>) -> anyhow::Result<String> {
    if machine.is_none() {
        tracing::warn!(
            path = %path.display(),
            "the machine's identifier could not be read; the daemon id is not checked against this machine"
        );
    }
    let existing = match std::fs::read(path) {
        Ok(bytes) => {
            let text = std::str::from_utf8(&bytes).map(str::trim);
            if let Ok(text) = text
                && Uuid::parse_str(text).is_ok()
            {
                Some(text.to_owned())
            } else {
                tracing::warn!(
                    path = %path.display(),
                    "the daemon id file is empty or not a UUID; replacing it with a new id"
                );
                None
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "no daemon id yet; creating one");
            None
        }
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", path.display()));
        }
    };
    if let Some(id) = &existing {
        let Some(machine) = machine else {
            return Ok(id.clone());
        };
        let stamp_path = stamp_of(path);
        match read_stamp(&stamp_path) {
            Ok(Some(stamp)) if stamp == stamp_digest(machine) => return Ok(id.clone()),
            Ok(Some(_)) => {
                tracing::info!(
                    path = %path.display(),
                    "the daemon id was minted on another machine; minting a new one"
                );
            }
            Ok(None) => {
                write_stamp(&stamp_path, machine);
                return Ok(id.clone());
            }
            Err(err) => {
                tracing::warn!(
                    path = %stamp_path.display(),
                    error = %err,
                    "the daemon id's machine stamp could not be read; the daemon id is not checked against this machine"
                );
                return Ok(id.clone());
            }
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let id = Uuid::new_v4().to_string();
    crate::atomic_file::write_atomic(path, id.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    if let Some(machine) = machine {
        write_stamp(&stamp_of(path), machine);
    }
    Ok(id)
}

/// This machine's identifier: Windows' `MachineGuid`, elsewhere `/etc/machine-id`.
/// `None` when it cannot be read or is empty.
#[cfg(windows)]
fn machine_identifier() -> Option<String> {
    crate::user_env::machine_guid()
}

/// This machine's identifier from `/etc/machine-id`, trimmed. `None` when the
/// file cannot be read or is empty.
#[cfg(not(windows))]
fn machine_identifier() -> Option<String> {
    std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// The stamp file beside the id file `path`: the machine identifier of the
/// machine that minted the id.
fn stamp_of(path: &Path) -> PathBuf {
    path.with_file_name("daemon-id.machine")
}

/// The trimmed stamp in `stamp`, or `None` when the file does not exist.
///
/// # Errors
///
/// The file exists but could not be read; the caller logs it and returns the
/// id unchecked.
fn read_stamp(stamp: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read(stamp) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).trim().to_owned())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", stamp.display())),
    }
}

/// Write `machine`'s digest to the stamp `stamp`. A write that fails is
/// logged: the daemon starts on its id either way, and the next start stamps
/// it.
fn write_stamp(stamp: &Path, machine: &str) {
    if let Err(err) = crate::atomic_file::write_atomic(stamp, stamp_digest(machine).as_bytes()) {
        tracing::warn!(
            path = %stamp.display(),
            error = %err,
            "the daemon id's machine stamp could not be written; it stays unstamped"
        );
    }
}

/// The stamp written for `identifier`: the lowercase hex SHA-256 of
/// `"rustling-tulip daemon-id:"` followed by the identifier, so the file never
/// holds the machine identifier itself.
fn stamp_digest(identifier: &str) -> String {
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(b"rustling-tulip daemon-id:");
    hasher.update(identifier.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use std::path::PathBuf;

    use super::{load_or_create, load_or_create_on, stamp_of};
    use sha2::{Digest as _, Sha256};
    use uuid::Uuid;

    /// The stamp digest an identifier is expected to produce, computed here so
    /// the assertion does not lean on the production helper.
    fn digest_of(identifier: &str) -> String {
        use std::fmt::Write as _;

        let mut hasher = Sha256::new();
        hasher.update(b"rustling-tulip daemon-id:");
        hasher.update(identifier.as_bytes());
        let mut out = String::with_capacity(64);
        for byte in hasher.finalize() {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

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

    #[test]
    fn an_id_without_a_stamp_is_kept_and_stamped() {
        let (root, file) = scratch_file("unstamped");
        let id = Uuid::new_v4().to_string();
        std::fs::write(&file, &id).expect("write a released build's id file");
        let kept = load_or_create_on(&file, Some("machine-a")).expect("an unstamped id is kept");
        assert_eq!(kept, id, "an id from an earlier release keeps its value");
        assert_eq!(
            std::fs::read_to_string(&file).expect("the id is still there"),
            id
        );
        assert_eq!(
            std::fs::read_to_string(stamp_of(&file)).expect("the stamp is written"),
            digest_of("machine-a"),
            "the stamp holds the digest of the machine that keeps the id"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_id_stamped_by_this_machine_is_kept() {
        let (root, file) = scratch_file("same-machine");
        let id = Uuid::new_v4().to_string();
        std::fs::write(&file, &id).expect("write the id file");
        std::fs::write(stamp_of(&file), digest_of("machine-a")).expect("write the stamp");
        let kept = load_or_create_on(&file, Some("machine-a")).expect("the id is kept");
        assert_eq!(kept, id);
        assert_eq!(
            std::fs::read_to_string(&file).expect("the id is still there"),
            id
        );
        assert_eq!(
            std::fs::read_to_string(stamp_of(&file)).expect("the stamp is still there"),
            digest_of("machine-a")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_id_stamped_by_another_machine_is_replaced() {
        let (root, file) = scratch_file("other-machine");
        let copied = Uuid::new_v4().to_string();
        std::fs::write(&file, &copied).expect("write the copied id file");
        std::fs::write(stamp_of(&file), digest_of("machine-a"))
            .expect("write the other machine's stamp");
        let fresh = load_or_create_on(&file, Some("machine-b")).expect("a copied id is replaced");
        Uuid::parse_str(&fresh).expect("the new id is a UUID");
        assert_ne!(fresh, copied, "the copied id is not kept");
        assert_eq!(
            std::fs::read_to_string(&file).expect("the new id is written"),
            fresh
        );
        assert_eq!(
            std::fs::read_to_string(stamp_of(&file)).expect("the stamp is rewritten"),
            digest_of("machine-b")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_fresh_start_writes_the_id_and_the_stamp() {
        let (root, file) = scratch_file("fresh");
        let id = load_or_create_on(&file, Some("machine-a")).expect("a fresh start mints an id");
        Uuid::parse_str(&id).expect("the new id is a UUID");
        assert_eq!(
            std::fs::read_to_string(&file).expect("the id is written"),
            id
        );
        assert_eq!(
            std::fs::read_to_string(stamp_of(&file)).expect("the stamp is written"),
            digest_of("machine-a")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_machine_identifier_keeps_todays_behaviour() {
        let (root, file) = scratch_file("no-machine-kept");
        let id = Uuid::new_v4().to_string();
        std::fs::write(&file, &id).expect("write the id file");
        let kept = load_or_create_on(&file, None).expect("a valid id is kept");
        assert_eq!(kept, id);
        assert!(
            !stamp_of(&file).exists(),
            "no stamp is written without a machine identifier"
        );
        let _ = std::fs::remove_dir_all(&root);

        let (root, file) = scratch_file("no-machine-minted");
        let minted = load_or_create_on(&file, None).expect("a missing id is minted");
        Uuid::parse_str(&minted).expect("the new id is a UUID");
        assert_eq!(
            std::fs::read_to_string(&file).expect("the id is written"),
            minted
        );
        assert!(
            !stamp_of(&file).exists(),
            "no stamp is written without a machine identifier"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stamp_that_cannot_be_written_still_returns_the_id() {
        let (root, file) = scratch_file("stamp-unwritable");
        std::fs::create_dir(stamp_of(&file)).expect("a folder where the stamp goes");
        let id = load_or_create_on(&file, Some("machine-a"))
            .expect("a stamp that cannot be written does not stop the daemon");
        Uuid::parse_str(&id).expect("the id is a UUID");
        assert_eq!(
            std::fs::read_to_string(&file).expect("the id is written"),
            id
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stamp_that_cannot_be_read_keeps_a_valid_id() {
        let (root, file) = scratch_file("stamp-unreadable");
        let id = Uuid::new_v4().to_string();
        std::fs::write(&file, &id).expect("write the id file");
        std::fs::create_dir(stamp_of(&file)).expect("a folder where the stamp goes");
        let kept = load_or_create_on(&file, Some("machine-a"))
            .expect("a stamp that cannot be read does not stop the daemon");
        assert_eq!(kept, id, "a valid id is kept unchecked");
        let _ = std::fs::remove_dir_all(&root);
    }
}
