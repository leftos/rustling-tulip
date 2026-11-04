//! Secrets never reach a `Debug` line or an error's text, and errors never name a credential path or a file's contents.

use std::error::Error;

use claude_accounts::{AccessToken, CredentialBlob, RefreshToken, Secret, StoreError};

use crate::support::{ALICE, ALICE_CRED, FAKE_TOKENS, FIXTURE_CREDENTIALS, Scratch};

fn assert_clean(text: &str, forbidden: &[&str]) {
    for word in forbidden {
        assert!(!text.contains(word), "{text:?} must not contain {word:?}");
    }
}

/// The error's text, its `Debug` and every source's text.
fn error_texts(error: &StoreError) -> Vec<String> {
    let mut texts = vec![error.to_string(), format!("{error:?}")];
    let mut source = error.source();
    while let Some(inner) = source {
        texts.push(inner.to_string());
        source = inner.source();
    }
    texts
}

#[test]
fn debug_redacts_every_secret() {
    let secret = Secret::new("fake-access".to_owned());
    let blob = CredentialBlob::new(FIXTURE_CREDENTIALS.to_owned());
    let access = AccessToken::new("fake-access".to_owned());
    let refresh = RefreshToken::new("fake-refresh".to_owned());
    assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
    assert_eq!(format!("{blob:?}"), "CredentialBlob(<redacted>)");
    assert_eq!(format!("{access:?}"), "AccessToken(<redacted>)");
    assert_eq!(format!("{refresh:?}"), "RefreshToken(<redacted>)");
    assert_eq!(secret.expose(), "fake-access");
    assert_eq!(access.expose(), "fake-access");
    assert_eq!(refresh.expose(), "fake-refresh");

    let scratch = Scratch::new("debug_redacts_every_secret");
    let store = scratch.store();
    let read = store.read_credentials(1).expect("the fixture reads");
    let read_debug = format!("{read:?} {store:?} {:#?}", Some(&blob));
    assert_clean(&read_debug, &FAKE_TOKENS);

    scratch.write(ALICE_CRED, "fake-access!");
    let error = store
        .read_credentials(1)
        .expect_err("junk is not a credential");
    for text in error_texts(&error) {
        assert_clean(&text, &FAKE_TOKENS);
    }
}

#[test]
fn error_text_has_no_path_or_contents() {
    let scratch = Scratch::new("error_text_has_no_path_or_contents");
    let store = scratch.store();
    let config = store
        .read_account_config(1)
        .expect("the fixture config reads")
        .expect("the fixture has a config");
    assert_eq!(config["emailAddress"], ALICE);

    let root = scratch.backup().to_string_lossy().into_owned();
    let forbidden = [
        root.as_str(),
        "1.cred",
        "credentials\\",
        "credentials/",
        "MARKER-CONTENT",
    ];

    scratch.write("accounts.json", r#"{"accounts": {"1": "MARKER-CONTENT"#);
    let sequence_error = store
        .read_sequence()
        .expect_err("a torn roster is an error");
    scratch.write(ALICE_CRED, "MARKER-CONTENT!!");
    let credentials_error = store
        .read_credentials(1)
        .expect_err("junk is not a credential");
    scratch.write("configs/1.json", r#"{"oauthAccount": "MARKER-CONTENT"#);
    let config_error = store
        .read_account_config(1)
        .expect_err("a torn config is an error");
    scratch.write("configs/1.json", r#"["MARKER-CONTENT"]"#);
    let shape_error = store
        .read_account_config(1)
        .expect_err("a config that is not an object is an error");

    for error in [
        &sequence_error,
        &credentials_error,
        &config_error,
        &shape_error,
    ] {
        for text in error_texts(error) {
            assert_clean(&text, &forbidden);
        }
    }
    assert_eq!(credentials_error.account(), Some(1));
    assert_eq!(config_error.account(), Some(1));
    assert_eq!(sequence_error.account(), None);
}

/// The config file a test writes, account 1's, whose identity is read and whose other key holds a secret that is not.
const SECRET_CONFIG: &str = r#"{"oauthAccount": {"emailAddress": "alice@example.com"}, "mcpServers": {"mine": {"env": {"API_KEY": "sk-test-secret"}}}}"#;

#[test]
fn an_account_config_debug_shows_no_value() {
    let scratch = Scratch::new("an_account_config_debug_shows_no_value");
    scratch.write("configs/1.json", SECRET_CONFIG);
    let store = scratch.store();
    let config = store
        .read_account_config(1)
        .expect("the config reads")
        .expect("the config is there");

    let debug = format!("{config:?}");
    assert_eq!(debug, "AccountConfig { .. }");
    assert_clean(&debug, &["sk-test-secret"]);
}

#[test]
fn an_account_config_reads_only_the_identity() {
    let scratch = Scratch::new("an_account_config_reads_only_the_identity");
    scratch.write("configs/1.json", SECRET_CONFIG);
    let store = scratch.store();
    let config = store
        .read_account_config(1)
        .expect("the config reads")
        .expect("the config is there");

    assert_eq!(config.get("emailAddress"), Some(&serde_json::json!(ALICE)));
    assert_eq!(config.len(), 1, "no other key of the live file is read");
}
