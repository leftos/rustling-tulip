//! What a relay push shows: its base64url ciphertext, from the notification's
//! `e` key, decrypted with the install's push keys into a [`PushNotice`] and
//! turned into the alert's title, body and badge. Any failure is a
//! [`Fallback`], on which the extension shows the alert as it came.

use base64::Engine;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use protocol::{PUSH_NOTICE_VERSION, PushNotice};
use push_crypto::InstallKeys;

/// URL-safe base64 read with or without `=` padding.
const URL_SAFE_ANY_PADDING: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// What the extension shows for a push that decrypted: the alert's title,
/// body and badge, and the ids it adds to the notification's user info.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    /// The alert's title.
    pub title: String,
    /// The alert's body.
    pub body: String,
    /// The app icon's badge.
    pub badge: u32,
    /// The tailnet name the app paired with, when the daemon sent one.
    pub host: Option<String>,
    /// The session the push is about.
    pub session_id: String,
}

/// Why a push shows its alert as it came. No variant carries a key, the
/// ciphertext or the plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// The push has no `e` ciphertext.
    NoCiphertext,
    /// The `e` ciphertext is not base64url.
    BadEncoding,
    /// The install's push keys are missing or not 48 octets.
    BadKeys,
    /// The ciphertext did not decrypt under the install's keys.
    Decrypt,
    /// The plaintext is not a push notice.
    Parse,
    /// The notice's version is newer than this build reads.
    NewerVersion,
}

/// Turns a push's `e` ciphertext into what the extension shows, reading the
/// install's push keys from `keys`, the Keychain item's raw octets (`None`
/// when the item is missing).
///
/// # Errors
///
/// The [`Fallback`] that says which step failed.
pub fn shown(e: Option<&str>, keys: Option<&[u8]>) -> Result<Shown, Fallback> {
    let e = e.ok_or(Fallback::NoCiphertext)?;
    let body = URL_SAFE_ANY_PADDING
        .decode(e)
        .map_err(|_| Fallback::BadEncoding)?;
    let keys = keys.ok_or(Fallback::BadKeys)?;
    let keys = InstallKeys::from_bytes(keys).map_err(|_| Fallback::BadKeys)?;
    let plaintext = push_crypto::decrypt(&body, &keys.private_key, &keys.auth)
        .map_err(|_| Fallback::Decrypt)?;
    let value: serde_json::Value =
        serde_json::from_slice(&plaintext).map_err(|_| Fallback::Parse)?;
    // A newer notice may not parse as this build's shape: its version decides first.
    let version = value.get("v").and_then(serde_json::Value::as_u64);
    if version.is_some_and(|v| v > u64::from(PUSH_NOTICE_VERSION)) {
        return Err(Fallback::NewerVersion);
    }
    let notice: PushNotice = serde_json::from_value(value).map_err(|_| Fallback::Parse)?;
    Ok(Shown {
        title: notice.title,
        body: notice.body,
        badge: notice.badge,
        host: notice.host,
        session_id: notice.session_id,
    })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{Fallback, Shown, shown};
    use base64::Engine;
    use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
    use protocol::{PUSH_NOTICE_VERSION, PushNotice, PushNoticeKind};
    use push_crypto::{InstallKeys, Subscriber};

    // RFC 8291 Appendix A's receiver keys.
    const UA_PUBLIC: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const UA_PRIVATE: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
    const AUTH_SECRET: &str = "BTBZMqHH6r4Tts7J_aSIgg";

    const MARKER: &str = "watermelon-marker";

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text).expect("valid base64url")
    }

    fn install_keys() -> InstallKeys {
        InstallKeys {
            private_key: b64(UA_PRIVATE).try_into().expect("32 octets"),
            auth: b64(AUTH_SECRET).try_into().expect("16 octets"),
        }
    }

    fn keys() -> Vec<u8> {
        install_keys().to_bytes().to_vec()
    }

    fn body(plaintext: &[u8]) -> Vec<u8> {
        let sub = Subscriber::from_b64url(UA_PUBLIC, AUTH_SECRET).expect("the RFC's keys");
        push_crypto::encrypt(&sub, plaintext).expect("encrypts")
    }

    fn sealed(plaintext: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(body(plaintext))
    }

    fn notice() -> PushNotice {
        PushNotice {
            v: PUSH_NOTICE_VERSION,
            kind: PushNoticeKind::NeedsYou,
            title: "Needs you".to_owned(),
            body: "fix the parser".to_owned(),
            session_id: "session-1".to_owned(),
            host: Some("desk.tail1234.ts.net".to_owned()),
            badge: 3,
            waited_since: None,
        }
    }

    fn sealed_notice(notice: &PushNotice) -> String {
        sealed(&serde_json::to_vec(notice).expect("a notice serializes"))
    }

    #[test]
    fn a_notice_decrypts_into_title_body_badge_and_ids() {
        let e = sealed_notice(&notice());
        assert_eq!(
            shown(Some(&e), Some(&keys())),
            Ok(Shown {
                title: "Needs you".to_owned(),
                body: "fix the parser".to_owned(),
                badge: 3,
                host: Some("desk.tail1234.ts.net".to_owned()),
                session_id: "session-1".to_owned(),
            })
        );
    }

    #[test]
    fn a_padded_ciphertext_reads() {
        // A body whose length is not a multiple of three ends in `=` padding.
        let mut notice = notice();
        let mut record = body(&serde_json::to_vec(&notice).expect("serializes"));
        while record.len().is_multiple_of(3) {
            notice.body.push('x');
            record = body(&serde_json::to_vec(&notice).expect("serializes"));
        }
        let padded = URL_SAFE.encode(&record);
        assert!(padded.ends_with('='), "{padded}");
        let read = shown(Some(&padded), Some(&keys())).expect("a padded ciphertext reads");
        assert_eq!(read.body, notice.body);
    }

    #[test]
    fn no_ciphertext_falls_back() {
        assert_eq!(shown(None, Some(&keys())), Err(Fallback::NoCiphertext));
    }

    #[test]
    fn bad_base64_falls_back() {
        assert_eq!(
            shown(Some("not base64url!"), Some(&keys())),
            Err(Fallback::BadEncoding)
        );
    }

    #[test]
    fn missing_keys_fall_back() {
        let e = sealed_notice(&notice());
        assert_eq!(shown(Some(&e), None), Err(Fallback::BadKeys));
    }

    #[test]
    fn short_keys_fall_back() {
        let e = sealed_notice(&notice());
        let keys = keys();
        let short = keys.get(..47).expect("48 octets");
        assert_eq!(shown(Some(&e), Some(short)), Err(Fallback::BadKeys));
    }

    #[test]
    fn the_wrong_auth_secret_falls_back() {
        let e = sealed_notice(&notice());
        let wrong = InstallKeys {
            auth: [0xab; 16],
            ..install_keys()
        };
        assert_eq!(
            shown(Some(&e), Some(&wrong.to_bytes())),
            Err(Fallback::Decrypt)
        );
    }

    #[test]
    fn bad_json_falls_back() {
        let e = sealed(b"not json");
        assert_eq!(shown(Some(&e), Some(&keys())), Err(Fallback::Parse));
    }

    #[test]
    fn a_newer_version_falls_back() {
        let newer = PushNotice {
            v: PUSH_NOTICE_VERSION + 1,
            ..notice()
        };
        let e = sealed_notice(&newer);
        assert_eq!(shown(Some(&e), Some(&keys())), Err(Fallback::NewerVersion));
    }

    #[test]
    fn an_unknown_kind_still_shows() {
        let json = format!(
            r#"{{"v":{PUSH_NOTICE_VERSION},"kind":"something_new","title":"Later","body":"a new kind","session_id":"session-2","badge":0}}"#
        );
        let e = sealed(json.as_bytes());
        let read = shown(Some(&e), Some(&keys())).expect("an unknown kind shows");
        assert_eq!(read.title, "Later");
        assert_eq!(read.body, "a new kind");
        assert_eq!(read.session_id, "session-2");
        assert_eq!(read.badge, 0);
    }

    #[test]
    fn a_missing_host_stays_missing() {
        let hostless = PushNotice {
            host: None,
            ..notice()
        };
        let e = sealed_notice(&hostless);
        let read = shown(Some(&e), Some(&keys())).expect("a hostless notice shows");
        assert_eq!(read.host, None);
        assert_eq!(read.session_id, "session-1");
    }

    #[test]
    fn fallback_debug_holds_no_plaintext() {
        let marked = PushNotice {
            v: PUSH_NOTICE_VERSION + 1,
            title: MARKER.to_owned(),
            body: MARKER.to_owned(),
            ..notice()
        };
        let newer = shown(Some(&sealed_notice(&marked)), Some(&keys()));
        let unparsed = shown(Some(&sealed(MARKER.as_bytes())), Some(&keys()));
        let wrong_keys = InstallKeys {
            auth: [0xab; 16],
            ..install_keys()
        };
        let undecrypted = shown(
            Some(&sealed(MARKER.as_bytes())),
            Some(&wrong_keys.to_bytes()),
        );
        let bad_keys = shown(Some(&sealed(MARKER.as_bytes())), Some(b"short"));
        for result in [newer, unparsed, undecrypted, bad_keys] {
            let err = result.expect_err("each case falls back");
            let debug = format!("{err:?}");
            assert!(
                !debug.contains(MARKER),
                "Debug prints the plaintext: {debug}"
            );
            assert!(
                debug.chars().all(|c| c.is_ascii_alphabetic()),
                "Debug carries a payload: {debug}"
            );
        }
    }
}
