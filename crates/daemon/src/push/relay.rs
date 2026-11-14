//! The push relay (`docs/plans/push-relay.md`): where a native phone's push
//! goes, and who the daemon says it is there. The relay takes the same
//! VAPID-signed, `aes128gcm`-encrypted POST a Web Push service takes, at a
//! path that names the platform and the device token.

use std::fmt::Write as _;

use reqwest::Url;
use sha2_011::{Digest, Sha256};

use super::store::{ApnsEnvironment, Channel};

/// The relay's origin: every `apns` and `fcm` push is posted under it.
pub(crate) const RELAY_ORIGIN: &str = "https://rt-push.leftos.dev";

/// The `sub` of a relay push's VAPID JWT: a fixed product URL, so the host's
/// tailnet name never reaches the relay.
pub(crate) const SUBJECT: &str = "https://github.com/leftos/rustling-tulip";

/// How many hex characters of a token's SHA-256 name it in a back-off key.
const FINGERPRINT_CHARS: usize = 12;

/// The relay URL a relay channel's push is posted to, under `origin`:
/// `/v1/apns/{production|sandbox}/{token}` or `/v1/fcm/{token}`, each segment
/// percent-encoded as a URL path segment. `None` for a Web Push channel, which
/// goes to its own endpoint, or an `origin` that cannot take a path.
pub(crate) fn relay_url(origin: &Url, channel: &Channel) -> Option<Url> {
    let segments: Vec<&str> = match channel {
        Channel::Apns {
            token, environment, ..
        } => vec!["v1", "apns", environment_name(*environment), token.as_str()],
        Channel::Fcm { token, .. } => vec!["v1", "fcm", token.as_str()],
        Channel::WebPush { .. } => return None,
    };
    let mut url = origin.clone();
    url.path_segments_mut().ok()?.clear().extend(segments);
    Some(url)
}

/// The environment as the relay's path names it: the store's `snake_case`
/// serde name.
fn environment_name(environment: ApnsEnvironment) -> &'static str {
    match environment {
        ApnsEnvironment::Production => "production",
        ApnsEnvironment::Sandbox => "sandbox",
    }
}

/// The first [`FINGERPRINT_CHARS`] hex characters of `token`'s SHA-256: it
/// tells one phone's back-off from another's without the token itself.
pub(crate) fn fingerprint(token: &str) -> String {
    let mut hex = String::with_capacity(FINGERPRINT_CHARS);
    for byte in Sha256::digest(token.as_bytes())
        .iter()
        .take(FINGERPRINT_CHARS / 2)
    {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use reqwest::Url;

    use super::{RELAY_ORIGIN, fingerprint, relay_url};
    use crate::push::store::{ApnsEnvironment, Channel};

    fn origin() -> Url {
        Url::parse(RELAY_ORIGIN).expect("the relay origin is a URL")
    }

    fn apns(token: &str, environment: ApnsEnvironment) -> Channel {
        Channel::Apns {
            token: token.to_owned(),
            environment,
            p256dh: String::new(),
            auth: String::new(),
        }
    }

    #[test]
    fn an_apns_production_token_maps_to_its_relay_path() {
        let url = relay_url(&origin(), &apns("0a1b2c3d", ApnsEnvironment::Production))
            .expect("a relay URL");

        assert_eq!(
            url.as_str(),
            "https://rt-push.leftos.dev/v1/apns/production/0a1b2c3d"
        );
    }

    #[test]
    fn a_sandbox_token_maps_to_the_sandbox_path() {
        let url =
            relay_url(&origin(), &apns("0a1b2c3d", ApnsEnvironment::Sandbox)).expect("a relay URL");

        assert_eq!(
            url.as_str(),
            "https://rt-push.leftos.dev/v1/apns/sandbox/0a1b2c3d"
        );
    }

    #[test]
    fn an_fcm_token_is_percent_encoded_in_its_path() {
        let token = "cZ1:APA91b/x%y z";
        let channel = Channel::Fcm {
            token: token.to_owned(),
            p256dh: String::new(),
            auth: String::new(),
        };

        let url = relay_url(&origin(), &channel).expect("a relay URL");

        assert_eq!(
            url.as_str(),
            "https://rt-push.leftos.dev/v1/fcm/cZ1:APA91b%2Fx%25y%20z",
            "a `:` is a path character; `/`, `%` and a space are encoded"
        );
        assert_eq!(url.path_segments().map(Iterator::count), Some(3));
    }

    #[test]
    fn web_push_has_no_relay_url() {
        let channel = Channel::WebPush {
            endpoint: "https://web.push.apple.com/abc".to_owned(),
            p256dh: String::new(),
            auth: String::new(),
        };

        assert_eq!(relay_url(&origin(), &channel), None);
    }

    #[test]
    fn a_fingerprint_is_twelve_hex_characters_of_the_token() {
        let print = fingerprint("0a1b2c3d");

        assert_eq!(print.len(), 12, "{print}");
        assert!(print.chars().all(|c| c.is_ascii_hexdigit()), "{print}");
        assert!(!print.contains("0a1b2c3d"));
        assert_ne!(print, fingerprint("0a1b2c3e"));
    }
}
