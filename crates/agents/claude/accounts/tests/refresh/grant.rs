//! The token POST and the credential fingerprints, against a fake token endpoint.

use std::fmt::Write as _;

use claude_accounts::{
    CLIENT_ID, CredentialBlob, RefreshError, TOKEN_URL, TransportError, access_token_fingerprint,
    credential_fingerprint, refresh_grant,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::support::{FakeEndpoint, answer, credential, granted, ms_after, now, with_account};

/// SHA-256 of `abc`, the FIPS 180-2 test vector.
const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn blob(text: &str) -> CredentialBlob {
    CredentialBlob::new(text.to_owned())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut text = String::new();
    for byte in &Sha256::digest(bytes) {
        write!(text, "{byte:02x}").expect("writing into a String cannot fail");
    }
    text
}

fn expired() -> String {
    credential("fake-access-one", Some("fake-refresh-one"), ms_after(-3600))
}

fn parsed(blob: &CredentialBlob) -> Value {
    serde_json::from_str(blob.expose()).expect("a refreshed credential is JSON")
}

#[test]
fn fingerprints_have_their_documented_form() {
    let with_refresh = credential("abc", Some("abc"), ms_after(0));
    assert_eq!(
        credential_fingerprint(&blob(&with_refresh)).as_deref(),
        Some(format!("sha256:{ABC}").as_str())
    );
    assert_eq!(
        access_token_fingerprint(&blob(&with_refresh)).as_deref(),
        Some(format!("sha256-at:{ABC}").as_str())
    );

    let fake = expired();
    let expected = format!("sha256:{}", hex_sha256(b"fake-refresh-one"));
    assert_eq!(
        credential_fingerprint(&blob(&fake)),
        Some(expected),
        "the refresh token alone decides an OAuth credential"
    );
    let expected = format!("sha256-at:{}", hex_sha256(b"fake-access-one"));
    assert_eq!(access_token_fingerprint(&blob(&fake)), Some(expected));

    assert_eq!(
        credential_fingerprint(&blob("abc")).as_deref(),
        Some(format!("sha256-full:{ABC}").as_str()),
        "an API key hashes whole"
    );
    let no_refresh = credential("fake-access-one", None, ms_after(0));
    let expected = format!("sha256-full:{}", hex_sha256(no_refresh.as_bytes()));
    assert_eq!(
        credential_fingerprint(&blob(&no_refresh)),
        Some(expected),
        "no refresh token hashes the whole text"
    );
    let empty_refresh = credential("fake-access-one", Some(""), ms_after(0));
    assert!(
        credential_fingerprint(&blob(&empty_refresh))
            .is_some_and(|fp| fp.starts_with("sha256-full:"))
    );

    assert_eq!(
        credential_fingerprint(&blob("")),
        None,
        "only empty text has no fingerprint"
    );
    assert_eq!(
        access_token_fingerprint(&blob("abc")),
        None,
        "an API key has no access token"
    );
    assert_eq!(
        access_token_fingerprint(&blob(&credential("", Some("abc"), 0))),
        None,
        "an empty access token has none"
    );
}

#[test]
fn refresh_posts_the_documented_body() {
    assert_eq!(TOKEN_URL, "https://platform.claude.com/v1/oauth/token");
    assert_eq!(CLIENT_ID, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);

    refresh_grant(&endpoint, &blob(&expired()), now()).expect("the grant is accepted");

    let expected = format!(
        r#"{{"grant_type":"refresh_token","refresh_token":"fake-refresh-one","client_id":"{CLIENT_ID}"}}"#
    );
    assert_eq!(endpoint.bodies(), vec![expected]);
}

#[test]
fn refresh_keeps_unknown_credential_keys_and_replaces_tokens() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);

    let refreshed = refresh_grant(&endpoint, &blob(&expired()), now())
        .expect("the grant is accepted")
        .credentials;

    let value = parsed(&refreshed);
    assert_eq!(
        value["mcpOAuth"],
        json!({ "keep": "me" }),
        "a key beside the OAuth block is kept"
    );
    let oauth = &value["claudeAiOauth"];
    assert_eq!(
        oauth["subscriptionType"], "max",
        "a key inside the OAuth block is kept"
    );
    assert_eq!(oauth["accessToken"], "fake-access-new");
    assert_eq!(oauth["refreshToken"], "fake-refresh-new");
    assert_eq!(
        oauth["expiresAt"],
        json!(ms_after(3600)),
        "expiresAt is now plus expires_in, in milliseconds"
    );
    assert_eq!(
        oauth["scopes"],
        json!(["user:inference", "user:profile"]),
        "scope is split on whitespace"
    );
    let keys: Vec<&String> = oauth
        .as_object()
        .expect("the OAuth block is an object")
        .keys()
        .collect();
    assert_eq!(
        keys,
        [
            "accessToken",
            "expiresAt",
            "scopes",
            "subscriptionType",
            "refreshToken"
        ],
        "existing keys keep their place"
    );
    assert!(
        !format!("{refreshed:?}").contains("fake-"),
        "the blob's Debug prints no token"
    );
}

#[test]
fn refresh_keeps_old_refresh_token_when_response_has_none() {
    let mut empty = granted("fake-access-new", None, 3600).expect("a canned answer");
    empty.body["refresh_token"] = json!("");
    let endpoint = FakeEndpoint::answering(vec![granted("fake-access-new", None, 3600), Ok(empty)]);

    for _ in 0..2 {
        let refreshed = refresh_grant(&endpoint, &blob(&expired()), now())
            .expect("the grant is accepted")
            .credentials;
        let value = parsed(&refreshed);
        assert_eq!(value["claudeAiOauth"]["refreshToken"], "fake-refresh-one");
        assert_eq!(value["claudeAiOauth"]["accessToken"], "fake-access-new");
    }
}

#[test]
fn invalid_grant_is_dead_lineage() {
    for status in [400, 401, 403] {
        let endpoint =
            FakeEndpoint::answering(vec![answer(status, json!({ "error": "invalid_grant" }))]);
        assert_eq!(
            refresh_grant(&endpoint, &blob(&expired()), now()).err(),
            Some(RefreshError::InvalidGrant),
            "status {status}"
        );
    }
    let endpoint = FakeEndpoint::answering(vec![answer(500, json!({ "error": "invalid_grant" }))]);
    assert_eq!(
        refresh_grant(&endpoint, &blob(&expired()), now()).err(),
        Some(RefreshError::Transient),
        "only 400/401/403 count"
    );
    let nested = json!({ "error": { "type": "x", "message": "invalid_grant" } });
    let endpoint = FakeEndpoint::answering(vec![answer(400, nested)]);
    assert_eq!(
        refresh_grant(&endpoint, &blob(&expired()), now()).err(),
        Some(RefreshError::Transient),
        "only a top-level error counts"
    );
    assert_eq!(RefreshError::InvalidGrant.kind(), "invalid_grant");
}

#[test]
fn invalid_client_is_systemic() {
    let endpoint = FakeEndpoint::answering(vec![answer(401, json!({ "error": "invalid_client" }))]);
    assert_eq!(
        refresh_grant(&endpoint, &blob(&expired()), now()).err(),
        Some(RefreshError::InvalidClient)
    );
    assert_eq!(RefreshError::InvalidClient.kind(), "invalid_client");
}

#[test]
fn unparseable_body_is_transient() {
    let missing_expiry = answer(200, json!({ "access_token": "fake-access-new" }));
    let bad_scope = answer(
        200,
        json!({ "access_token": "fake-access-new", "expires_in": 3600, "scope": ["user:inference"] }),
    );
    let answers = vec![
        answer(400, Value::Null),
        answer(200, Value::Null),
        Err(TransportError::MalformedBody),
        Err(TransportError::Timeout),
        missing_expiry,
        bad_scope,
    ];
    let count = answers.len();
    let endpoint = FakeEndpoint::answering(answers);
    for attempt in 0..count {
        let result = refresh_grant(&endpoint, &blob(&expired()), now());
        assert_eq!(
            result.err(),
            Some(RefreshError::Transient),
            "answer {attempt}"
        );
    }
    assert_eq!(RefreshError::Transient.kind(), "transient");
}

/// With no new refresh token the old one is still the live one, so an answer without a usable access token is no
/// grant; with a new one, the same answer is a successor that must be kept.
#[test]
fn answer_without_a_new_refresh_token_and_no_access_token_is_transient() {
    let answers = vec![
        answer(200, json!({ "expires_in": 3600 })),
        answer(200, json!({ "access_token": "", "expires_in": 3600 })),
        answer(
            200,
            json!({ "access_token": null, "expires_in": 3600, "refresh_token": "" }),
        ),
        answer(
            200,
            json!({ "access_token": "fake-access-new", "expires_in": "3600" }),
        ),
    ];
    let count = answers.len();
    let endpoint = FakeEndpoint::answering(answers);
    for attempt in 0..count {
        let result = refresh_grant(&endpoint, &blob(&expired()), now());
        assert_eq!(
            result.err(),
            Some(RefreshError::Transient),
            "answer {attempt}"
        );
    }

    let endpoint = FakeEndpoint::answering(vec![answer(
        200,
        json!({ "expires_in": 3600, "refresh_token": "fake-refresh-new" }),
    )]);
    let successor = refresh_grant(&endpoint, &blob(&expired()), now())
        .expect("a new refresh token is always a successor")
        .credentials;
    let oauth = &parsed(&successor)["claudeAiOauth"];
    assert_eq!(oauth["refreshToken"], "fake-refresh-new");
    assert_eq!(oauth["accessToken"], "");
    assert_eq!(
        oauth["expiresAt"],
        ms_after(0),
        "the successor sorts after the spent copy and reads as expired: the refresh's clock, later than the spent expiry"
    );
}

/// A successor whose answer gives no expiry stands one millisecond after the spent copy's `expiresAt` when that is
/// later than the refresh's clock, so it still sorts after what it spent.
#[test]
fn successor_without_an_expiry_sorts_after_an_unexpired_spent_copy() {
    let spent = credential("fake-access-one", Some("fake-refresh-one"), ms_after(600));
    let endpoint = FakeEndpoint::answering(vec![answer(
        200,
        json!({ "access_token": "fake-access-new", "refresh_token": "fake-refresh-new" }),
    )]);

    let successor = refresh_grant(&endpoint, &blob(&spent), now())
        .expect("a new refresh token is always a successor")
        .credentials;

    assert_eq!(
        parsed(&successor)["claudeAiOauth"]["expiresAt"],
        ms_after(600) + 1
    );
}

#[test]
fn no_refresh_token_is_permanent_without_a_post() {
    let endpoint = FakeEndpoint::answering(vec![granted("fake-access-new", None, 3600)]);

    let no_refresh = credential("fake-access-one", None, ms_after(-3600));
    assert_eq!(
        refresh_grant(&endpoint, &blob(&no_refresh), now()).err(),
        Some(RefreshError::NoRefreshToken)
    );
    let no_block = json!({ "mcpOAuth": {} }).to_string();
    assert_eq!(
        refresh_grant(&endpoint, &blob(&no_block), now()).err(),
        Some(RefreshError::NoRefreshToken)
    );
    assert_eq!(
        refresh_grant(&endpoint, &blob("not json"), now()).err(),
        Some(RefreshError::Transient),
        "a torn read is transient"
    );
    assert_eq!(RefreshError::NoRefreshToken.kind(), "no_refresh_token");

    assert!(endpoint.bodies().is_empty(), "nothing was POSTed");
}

#[test]
fn token_account_debug_hides_the_email() {
    let endpoint = FakeEndpoint::answering(vec![
        with_account(
            granted("fake-access-new", Some("fake-refresh-new"), 3600),
            json!({ "uuid": " uuid-one ", "email_address": "secret@example.test" }),
            json!({ "uuid": "org-one" }),
        ),
        granted("fake-access-new", Some("fake-refresh-new"), 3600),
    ]);

    let refreshed =
        refresh_grant(&endpoint, &blob(&expired()), now()).expect("the grant is accepted");
    let account = refreshed
        .token_account
        .expect("the answer names an account");
    assert_eq!(
        account.account_uuid.as_deref(),
        Some("uuid-one"),
        "the uuid is trimmed"
    );
    assert_eq!(account.email.as_deref(), Some("secret@example.test"));
    assert_eq!(account.organization_uuid.as_deref(), Some("org-one"));
    let debug = format!("{account:?}");
    assert!(
        debug.contains("uuid-one") && debug.contains("org-one"),
        "the uuids print: {debug}"
    );
    assert!(
        !debug.contains("secret@example.test"),
        "the email never prints: {debug}"
    );
    assert!(debug.contains("<redacted>"), "{debug}");

    let refreshed =
        refresh_grant(&endpoint, &blob(&expired()), now()).expect("the grant is accepted");
    assert_eq!(
        refreshed.token_account, None,
        "an answer naming no account carries no token account"
    );
}

#[cfg(accounts_locks)]
mod gate {
    use claude_accounts::StashReason;
    use serde_json::json;

    use crate::support::{
        FakeEndpoint, Rig, Scratch, credential, granted, ms_after, put_credentials, with_account,
    };

    fn rig(test: &str, stored: &str) -> Rig {
        let scratch = Scratch::new(test);
        scratch.set_active(2);
        let endpoint = FakeEndpoint::answering(vec![with_account(
            granted("fake-access-new", Some("fake-refresh-new"), 3600),
            json!({ "uuid": "uuid-one", "email_address": "one@example.test" }),
            json!({ "uuid": "org-one" }),
        )]);
        let rig = Rig::new(scratch, endpoint);
        put_credentials(&rig.scratch.root(), stored);
        rig
    }

    #[test]
    fn a_spent_grant_carries_the_token_account() {
        let stored = credential("fake-access-one", Some("fake-refresh-one"), ms_after(-3600));
        let rig = rig("grant_token_account_spent", &stored);

        let outcome = rig.consume(&stored);

        assert_eq!(outcome.error, None);
        let account = outcome
            .token_account
            .expect("the spent grant names its account");
        assert_eq!(account.account_uuid.as_deref(), Some("uuid-one"));
        assert_eq!(account.organization_uuid.as_deref(), Some("org-one"));
    }

    #[test]
    fn an_adopted_grant_carries_no_token_account() {
        let stored = credential(
            "fake-access-stored",
            Some("fake-refresh-one"),
            ms_after(3600),
        );
        let rig = rig("grant_token_account_adopted", &stored);

        let outcome = rig.consume(&credential(
            "fake-access-old",
            Some("fake-refresh-one"),
            ms_after(-3600),
        ));

        assert_eq!(outcome.error, None);
        assert!(
            outcome.credentials.is_some(),
            "the store's fresher credential is handed back"
        );
        assert!(rig.endpoint.bodies().is_empty(), "nothing was spent");
        assert_eq!(outcome.token_account, None);
    }

    #[test]
    fn a_cas_conflict_adoption_still_carries_the_token_account() {
        let stored = credential("fake-access-one", Some("fake-refresh-one"), ms_after(-3600));
        let other = credential(
            "fake-access-other",
            Some("fake-refresh-other"),
            ms_after(7200),
        );
        let scratch = Scratch::new("grant_token_account_cas");
        scratch.set_active(2);
        put_credentials(&scratch.root(), &stored);
        let root = scratch.root();
        let written = other.clone();
        let endpoint = FakeEndpoint::answering(vec![with_account(
            granted("fake-access-new", Some("fake-refresh-new"), 3600),
            json!({ "uuid": "uuid-one" }),
            json!({ "uuid": "org-one" }),
        )])
        .during_post(move || put_credentials(&root, &written));
        let rig = Rig::new(scratch, endpoint);

        let outcome = rig.consume(&stored);

        assert_eq!(outcome.stash, Some(StashReason::CasConflict));
        assert_eq!(
            outcome
                .credentials
                .expect("the racing writer's credential is adopted")
                .expose(),
            other
        );
        let account = outcome
            .token_account
            .expect("the spent grant's account rides along");
        assert_eq!(account.account_uuid.as_deref(), Some("uuid-one"));
    }
}
