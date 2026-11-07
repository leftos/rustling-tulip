//! Web Push to the phone app: the payload encrypted for one subscription
//! (RFC 8291, `aes128gcm`), the VAPID JWT that identifies the daemon to the
//! push service (RFC 8292, ES256), and the POST that delivers it, with the
//! push service's answer sorted into what the caller does next
//! (`docs/plans/mobile-app.md`, "Push").

pub(crate) mod send;
pub(crate) mod vapid;
pub(crate) mod webpush;
