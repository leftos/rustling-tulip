//! `credentials/*.cred`: a sealed file bound to its account, its organization, its generation and its email, read
//! through the guard, with one previous generation kept as a raw ciphertext copy, and `configs/*.json` holding only the
//! account's `oauthAccount`.

use std::fs;

use claude_accounts::guard;
use claude_accounts::seal::{self, AssociatedData, SealPurpose};
use claude_accounts::{CredentialBlob, Retention, StoreErrorKind};
use serde_json::json;

use crate::support::{ALICE, ALICE_CRED, ALICE_PREV, BOB, FIXTURE_CREDENTIALS, Scratch};

const NEXT: &str =
    r#"{"claudeAiOauth":{"accessToken":"fake-access-2","refreshToken":"fake-refresh-2"}}"#;
const ALICE_CONFIG: &str = "configs/1.json";

fn blob(text: &str) -> CredentialBlob {
    CredentialBlob::new(text.to_owned())
}

/// The AD account `number`'s ledger entry names at `generation`.
fn ad(scratch: &Scratch, number: u32, generation: u64) -> AssociatedData {
    let roster = scratch.read_json("accounts.json");
    let record = &roster["accounts"][number.to_string()];
    let organization = record["orgUuid"].as_str().unwrap_or_default();
    let identity = record["email"]
        .as_str()
        .expect("the fixture records an email");
    AssociatedData::credentials(number, organization, generation, identity)
}

fn temp_files(scratch: &Scratch) -> Vec<String> {
    fs::read_dir(scratch.path("credentials"))
        .expect("the credentials folder lists")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
        })
        .collect()
}

#[test]
fn sealed_round_trip_reads_back_the_same_credentials() {
    let scratch = Scratch::new("sealed_round_trip");
    let store = scratch.store();

    let read = store
        .read_credentials(1)
        .expect("the fixture reads")
        .expect("the fixture has credentials");

    assert_eq!(read.expose(), FIXTURE_CREDENTIALS);
    let bytes = scratch.read(ALICE_CRED);
    assert!(
        bytes.starts_with(b"OTSEAL"),
        "the file is a sealed file, not base64: {bytes:?}"
    );
    assert_eq!(
        temp_files(&scratch),
        Vec::<String>::new(),
        "the temp file is gone after the replace"
    );
    #[cfg(windows)]
    assert!(
        !bytes
            .windows(ALICE.len())
            .any(|window| window == ALICE.as_bytes()),
        "a sealed file carries no account email"
    );
}

#[test]
fn a_write_for_a_number_the_roster_does_not_name_is_refused() {
    let scratch = Scratch::new("a_write_for_an_unknown_number_is_refused");
    let store = scratch.store();
    let guard = scratch.lock();

    let error = store
        .write_credentials(&guard, 7, &blob(NEXT))
        .expect_err("a number no record names has no identity to seal under");

    assert_eq!(error.kind(), StoreErrorKind::UnknownAccount);
    assert!(!scratch.exists("credentials/7.cred"), "nothing is written");
}

#[test]
fn a_file_sealed_for_another_slot_is_refused() {
    let scratch = Scratch::new("sealed_for_another_slot");
    let other = seal::seal(
        SealPurpose::Credentials,
        &AssociatedData::credentials(2, "", 1, BOB),
        &blob(NEXT),
    )
    .expect("the other slot's file seals");
    scratch.write_bytes(ALICE_CRED, other.as_bytes());

    let error = scratch
        .store()
        .read_credentials(1)
        .expect_err("another slot's file is not account 1's");

    assert_eq!(error.kind(), StoreErrorKind::CredentialsUnopenable);
    assert_eq!(
        scratch.read(ALICE_CRED),
        other.as_bytes(),
        "the file is left as it was"
    );
}

#[test]
fn a_foreign_file_is_refused_by_read_and_never_overwritten() {
    let scratch = Scratch::new("foreign_file_refused");
    let base64 = b"eyJjbGF1ZGVBaU9hdXRoIjp7fX0=";
    scratch.write_bytes(ALICE_CRED, base64);
    let store = scratch.store();

    let read = store
        .read_credentials(1)
        .expect_err("a cswap file is not a sealed one");
    let guard = scratch.lock();
    let write = store
        .write_credentials(&guard, 1, &blob(NEXT))
        .expect_err("a foreign file is never overwritten");

    assert_eq!(read.kind(), StoreErrorKind::Foreign);
    assert_eq!(write.kind(), StoreErrorKind::Foreign);
    assert_eq!(scratch.read(ALICE_CRED), base64, "the bytes are unchanged");
    assert!(!scratch.exists(ALICE_PREV), "no .prev is written beside it");
}

#[test]
fn a_zero_length_file_is_an_error_not_absent() {
    let scratch = Scratch::new("zero_length_credentials");
    scratch.write_bytes(ALICE_CRED, b"");

    let error = scratch
        .store()
        .read_credentials(1)
        .expect_err("an empty file is no credential");

    assert_eq!(error.kind(), StoreErrorKind::Foreign);
}

#[test]
fn a_missing_file_is_absent() {
    let scratch = Scratch::new("missing_credentials");
    fs::remove_file(scratch.path(ALICE_CRED)).expect("the credential file can be removed");

    assert!(
        scratch
            .store()
            .read_credentials(1)
            .expect("a missing file is not an error")
            .is_none()
    );
}

#[test]
fn a_write_keeps_the_replaced_ciphertext_byte_for_byte_as_prev() {
    let scratch = Scratch::new("prev_is_raw_ciphertext");
    let store = scratch.store();
    let before = scratch.read(ALICE_CRED);
    let guard = scratch.lock();

    let retention = store
        .write_credentials(&guard, 1, &blob(NEXT))
        .expect("the write lands");

    assert_eq!(retention, Retention::Kept);
    assert_eq!(
        scratch.read(ALICE_PREV),
        before,
        "the previous generation is the replaced ciphertext, unchanged"
    );
    let prev = guard::read_file(&scratch.backup(), &scratch.path(ALICE_PREV))
        .expect("the guard reads .prev");
    let generation = prev.generation().expect(".prev is a sealed file");
    let opened = seal::open(
        SealPurpose::Credentials,
        &ad(&scratch, 1, generation),
        &prev,
    )
    .expect(".prev opens with its own AD");
    assert_eq!(
        opened.expose(),
        FIXTURE_CREDENTIALS,
        "it opens as the generation it replaced"
    );
    let current = store
        .read_credentials(1)
        .expect("the new file reads")
        .expect("the new file holds credentials");
    assert_eq!(current.expose(), NEXT);
}

#[test]
fn an_unchanged_plaintext_keeps_no_new_prev() {
    let scratch = Scratch::new("unchanged_plaintext_no_prev");
    let store = scratch.store();
    let guard = scratch.lock();

    let retention = store
        .write_credentials(&guard, 1, &blob(FIXTURE_CREDENTIALS))
        .expect("the same write lands");

    assert_eq!(retention, Retention::NothingToKeep);
    assert!(
        !scratch.exists(ALICE_PREV),
        "nothing was replaced, so nothing is kept"
    );
}

#[test]
fn a_read_back_failure_leaves_the_old_file_and_no_temp() {
    let scratch = Scratch::new("read_back_failure");
    let before = scratch.read(ALICE_CRED);
    let store = scratch.store().with_failed_read_back();
    let guard = scratch.lock();

    let error = store
        .write_credentials(&guard, 1, &blob(NEXT))
        .expect_err("the read-back fails the write");

    assert_eq!(error.kind(), StoreErrorKind::CredentialsWrite);
    assert_eq!(
        scratch.read(ALICE_CRED),
        before,
        "the old file is unchanged"
    );
    assert_eq!(
        temp_files(&scratch),
        Vec::<String>::new(),
        "the temp file is removed"
    );
}

#[test]
fn an_unopenable_current_file_is_refused_and_kept() {
    let scratch = Scratch::new("unopenable_current_refused");
    let mut corrupted = scratch.read(ALICE_CRED);
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0x01;
    scratch.write_bytes(ALICE_CRED, &corrupted);
    let store = scratch.store();
    let guard = scratch.lock();

    let error = store
        .write_credentials(&guard, 1, &blob(NEXT))
        .expect_err("an unopenable file is never overwritten");

    assert_eq!(error.kind(), StoreErrorKind::CredentialsUnopenable);
    assert_eq!(
        scratch.read(ALICE_CRED),
        corrupted,
        "the bytes are unchanged"
    );
}

#[test]
fn a_rolled_back_credential_file_is_refused() {
    let scratch = Scratch::new("rolled_back_credentials");
    let store = scratch.store();
    let old = scratch.read(ALICE_CRED);
    let guard = scratch.lock();
    store
        .write_credentials(&guard, 1, &blob(NEXT))
        .expect("a second generation lands");

    scratch.write_bytes(ALICE_CRED, &old);
    let error = store
        .read_credentials(1)
        .expect_err("an older file is refused");

    assert_eq!(error.kind(), StoreErrorKind::CredentialsRolledBack);
}

#[test]
fn a_failing_canary_refuses_every_write_and_reads_still_work() {
    let scratch = Scratch::new("failing_canary_refuses");
    let store = scratch.store_without_sealing();
    let guard = scratch.lock();
    let before = [
        scratch.read("accounts.json"),
        scratch.read(ALICE_CRED),
        scratch.read(ALICE_CONFIG),
    ];
    let sequence = store
        .read_sequence()
        .expect("a read works")
        .expect("the roster exists");

    assert_eq!(store.write_refusal(), Some(StoreErrorKind::StoreReadOnly));
    assert!(store.read_credentials(1).expect("a read works").is_some());
    assert!(
        store
            .read_account_config(1)
            .expect("a read works")
            .is_some()
    );
    let writes = [
        ("set_active", store.set_active(&guard, 2).err()),
        (
            "write_sequence",
            store.write_sequence(&guard, &sequence).err(),
        ),
        (
            "write_credentials",
            store.write_credentials(&guard, 1, &blob(NEXT)).err(),
        ),
        (
            "write_account_config",
            store
                .write_account_config(&guard, 1, &json!({"emailAddress": ALICE}))
                .err(),
        ),
    ];
    for (write, error) in writes {
        assert_eq!(
            error.map(|error| error.kind()),
            Some(StoreErrorKind::StoreReadOnly),
            "{write}"
        );
    }
    assert_eq!(
        [
            scratch.read("accounts.json"),
            scratch.read(ALICE_CRED),
            scratch.read(ALICE_CONFIG)
        ],
        before,
        "every byte is unchanged"
    );
}

#[test]
fn a_passing_canary_lifts_the_refusal() {
    let scratch = Scratch::new("passing_canary");
    let store = scratch.store();

    assert_eq!(store.write_refusal(), None, "sealing works in this process");
    let guard = scratch.lock();
    store
        .write_credentials(&guard, 1, &blob(NEXT))
        .expect("the write goes through");
}

#[test]
fn a_config_write_holds_only_the_oauth_account() {
    let scratch = Scratch::new("config_holds_only_identity");
    let store = scratch.store();
    let guard = scratch.lock();
    let identity = json!({"emailAddress": ALICE, "organizationUuid": "org-a"});

    store
        .write_account_config(&guard, 1, &identity)
        .expect("the identity writes");

    let written = scratch.read_json(ALICE_CONFIG);
    assert_eq!(
        written,
        json!({"oauthAccount": identity}),
        "no other key reaches the file"
    );
}

#[test]
fn a_config_read_returns_only_the_oauth_account() {
    let scratch = Scratch::new("config_read_only_identity");
    scratch.write(
        ALICE_CONFIG,
        r#"{"oauthAccount": {"emailAddress": "alice@example.com"}, "mcpServers": {"keep": "not-read"}}"#,
    );

    let config = scratch
        .store()
        .read_account_config(1)
        .expect("the config reads")
        .expect("the config is there");

    assert_eq!(config.get("emailAddress"), Some(&json!(ALICE)));
    assert_eq!(config.len(), 1, "the other keys are not read");
}

#[test]
fn a_config_write_of_a_non_object_is_refused() {
    let scratch = Scratch::new("config_non_object_refused");
    let store = scratch.store();
    let guard = scratch.lock();
    let before = scratch.read(ALICE_CONFIG);

    let error = store
        .write_account_config(&guard, 1, &json!("not-an-identity"))
        .expect_err("only an identity object writes");

    assert_eq!(error.kind(), StoreErrorKind::ConfigWrite);
    assert_eq!(scratch.read(ALICE_CONFIG), before, "the file is unchanged");
}

#[test]
fn a_config_without_an_identity_is_corrupt() {
    let scratch = Scratch::new("config_without_identity");
    scratch.write(ALICE_CONFIG, r#"{"mcpServers": {}}"#);

    let error = scratch
        .store()
        .read_account_config(1)
        .expect_err("no identity, no config");

    assert_eq!(error.kind(), StoreErrorKind::ConfigCorrupt);
}
