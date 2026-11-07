//! Web Push to the phone app: the payload encrypted for one subscription
//! (RFC 8291, `aes128gcm`), the VAPID JWT that identifies the daemon to the
//! push service (RFC 8292, ES256), and the POST that delivers it, with the
//! push service's answer sorted into what the caller does next
//! (`docs/plans/mobile-app.md`, "Push").

use std::path::Path;

use anyhow::Context as _;

pub(crate) mod keys;
pub(crate) mod send;
pub(crate) mod state;
pub(crate) mod store;
pub(crate) mod vapid;
pub(crate) mod webpush;

/// Create the push folder `dir` if it is missing. On Unix it is owner-only,
/// so the VAPID private key and the device subscriptions stay private to the
/// user.
///
/// # Errors
///
/// The folder could not be created or, on Unix, made owner-only.
pub(crate) fn ensure_folder(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating push dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("making push dir {} owner-only", dir.display()))?;
    }
    Ok(())
}
