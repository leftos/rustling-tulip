//! The daemon's VAPID key pair on disk: `push/vapid.json`, created on first
//! use and never rotated, so nothing silently invalidates the devices'
//! subscriptions. A corrupt file is reported and left as it is; deleting the
//! folder is the only reset (`docs/plans/mobile-app.md`, "Push").

use std::io;
use std::path::Path;

use anyhow::{Context as _, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use super::vapid::VapidKey;

/// The key file's name inside the push folder.
const KEY_FILE: &str = "vapid.json";

/// `vapid.json`: the base64url private scalar, without padding.
#[derive(Serialize, Deserialize)]
struct VapidFile {
    private_key: String,
}

/// The key stored in `dir`, creating and saving a new one when the file is
/// missing. Callers serialize it through one lock: two first calls at once
/// would each generate a key, and the last write wins.
///
/// # Errors
///
/// The file exists but cannot be read or decoded — it is left untouched, so a
/// corrupt key is never replaced by a fresh one — or a new key could not be
/// written.
pub(crate) fn load_or_create(dir: &Path) -> anyhow::Result<VapidKey> {
    let path = dir.join(KEY_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => decode(&bytes, &path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => create(&path),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn decode(bytes: &[u8], path: &Path) -> anyhow::Result<VapidKey> {
    let file: VapidFile =
        serde_json::from_slice(bytes).with_context(|| format!("decoding {}", path.display()))?;
    let raw = URL_SAFE_NO_PAD
        .decode(file.private_key.as_bytes())
        .with_context(|| format!("decoding {}", path.display()))?;
    let raw: [u8; 32] = raw.as_slice().try_into().map_err(|_| {
        anyhow!(
            "decoding {}: the stored key is {} bytes, not 32",
            path.display(),
            raw.len()
        )
    })?;
    VapidKey::from_raw(&raw).with_context(|| format!("decoding {}", path.display()))
}

fn create(path: &Path) -> anyhow::Result<VapidKey> {
    let key = VapidKey::generate()?;
    let bytes = serde_json::to_vec(&VapidFile {
        private_key: URL_SAFE_NO_PAD.encode(key.to_raw()),
    })
    .with_context(|| format!("serializing {}", path.display()))?;
    crate::atomic_file::write_atomic(path, &bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(key)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and panic"
)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{KEY_FILE, load_or_create};

    /// A scratch push folder, removed on drop.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("rt-push-keys-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self { dir }
        }

        fn dir(&self) -> &Path {
            &self.dir
        }

        fn file(&self) -> PathBuf {
            self.dir.join(KEY_FILE)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn the_first_call_creates_the_file_and_the_second_returns_the_same_key() {
        let scratch = Scratch::new("same-key");

        let first = load_or_create(scratch.dir()).expect("create the key");
        assert!(
            scratch.file().is_file(),
            "the first call writes {}",
            scratch.file().display()
        );

        let second = load_or_create(scratch.dir()).expect("load the stored key");
        assert_eq!(
            first.public_b64url(),
            second.public_b64url(),
            "a second call returns the stored key, not a fresh one"
        );
    }

    #[test]
    fn a_corrupt_file_is_an_error_naming_it_and_is_left_untouched() {
        let scratch = Scratch::new("corrupt");
        let corrupted = b"{ not the key file";
        std::fs::write(scratch.file(), corrupted).expect("write a corrupt vapid.json");

        let Err(err) = load_or_create(scratch.dir()) else {
            panic!("a corrupt vapid.json is an error");
        };

        let message = format!("{err:#}");
        assert!(
            message.contains(&scratch.file().display().to_string()),
            "{message} names the file"
        );
        assert_eq!(
            std::fs::read(scratch.file()).expect("read the file back"),
            corrupted,
            "the corrupt file is never overwritten"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let scratch = Scratch::new("mode");
        load_or_create(scratch.dir()).expect("create the key");

        let mode = std::fs::metadata(scratch.file())
            .expect("read the key file's metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the private key is the owner's alone");
    }
}
