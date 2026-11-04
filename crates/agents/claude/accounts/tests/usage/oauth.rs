//! `CredentialBlob::oauth`: the `claudeAiOauth` block, read into secret types that never print.

use claude_accounts::CredentialBlob;

#[cfg(all(windows, target_pointer_width = "64"))]
use crate::support::{FAKE_TOKENS, Scratch};

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn oauth_debug_hides_tokens() {
    let scratch = Scratch::new("oauth_debug_hides_tokens");
    let blob = scratch
        .store()
        .read_credentials(1)
        .expect("account 1's credentials read")
        .expect("account 1 has credentials");

    let oauth = blob.oauth();

    let credentials = oauth
        .as_ref()
        .expect("account 1's credentials hold an OAuth block");
    assert!(
        credentials.refresh_token().is_some(),
        "the refresh token is read"
    );
    assert_eq!(
        credentials
            .expires_at()
            .map(jiff::Timestamp::as_millisecond),
        Some(4_102_444_800_000)
    );
    assert_eq!(
        credentials
            .refresh_token_expires_at()
            .map(jiff::Timestamp::as_millisecond),
        Some(4_133_980_800_000)
    );
    let printed = format!("{oauth:?}");
    assert!(
        printed.contains("<redacted>"),
        "the debug output says what it hides"
    );
    for token in FAKE_TOKENS {
        assert!(
            !printed.contains(token),
            "the debug output of the OAuth credentials names no token"
        );
    }
}

#[test]
fn oauth_missing_gives_none() {
    for text in [
        // No OAuth block.
        r#"{"trustedDeviceToken":"fake-device"}"#,
        // A managed API key is not JSON.
        "sk-ant-api03-fake-key",
        // An OAuth block without an access token, or with an empty one.
        r#"{"claudeAiOauth":{"refreshToken":"fake-refresh"}}"#,
        r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"fake-refresh"}}"#,
        // An OAuth block that is not an object.
        r#"{"claudeAiOauth":"fake-access"}"#,
    ] {
        assert!(
            CredentialBlob::new(text.to_owned()).oauth().is_none(),
            "a credential without a usable OAuth block gives none"
        );
    }

    let minimal = CredentialBlob::new(
        r#"{"claudeAiOauth":{"accessToken":"fake-access","expiresAt":"soon"}}"#.to_owned(),
    );
    let oauth = minimal.oauth().expect("an access token alone is enough");
    assert!(oauth.refresh_token().is_none());
    assert!(
        oauth.expires_at().is_none(),
        "an expiry that is not a number is unknown"
    );
}
