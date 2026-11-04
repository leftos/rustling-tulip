//! `Store::add_accounts`: new accounts' records, config files and sealed credential files, with every check made
//! before any write.

use claude_accounts::seal::{self, AssociatedData, SealPurpose};
use claude_accounts::{
    AccountKind, AccountNumber, CredentialBlob, Email, NewAccount, StoreErrorKind,
};
use serde_json::{Value, json};

use crate::support::{ALICE, BOB, Scratch};

const CAROL: &str = "carol@example.com";
const CAROL_ORG: &str = "00000000-0000-4000-8000-0000000000c1";
const CAROL_UUID: &str = "00000000-0000-4000-8000-000000000003";
const DAVE: &str = "dave@example.com";
/// Account 1's organization in the store fixture.
const ALICE_ORG: &str = "00000000-0000-4000-8000-0000000000a1";
const CAROL_CREDENTIALS: &str =
    r#"{"claudeAiOauth":{"accessToken":"fake-access-carol","refreshToken":"fake-refresh-carol"}}"#;

fn blob(text: &str) -> CredentialBlob {
    CredentialBlob::new(text.to_owned())
}

/// An `oauthAccount` naming `email` in `org` with `uuid`.
fn oauth(email: &str, org: &str, uuid: &str) -> Value {
    json!({"emailAddress": email, "organizationUuid": org, "accountUuid": uuid, "displayName": "someone"})
}

fn account<'a>(
    number: AccountNumber,
    email: &str,
    org: &str,
    oauth_account: &'a Value,
    credentials: &'a CredentialBlob,
) -> NewAccount<'a> {
    NewAccount {
        number,
        email: Email::parse(email).expect("the test email is safe"),
        uuid: oauth_account["accountUuid"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        org_uuid: org.to_owned(),
        org_name: String::new(),
        added: "2026-10-23T10:00:00Z".to_owned(),
        alias: None,
        disabled: false,
        kind: AccountKind::Oauth,
        oauth_account,
        credentials,
    }
}

/// Carol, with her `oauthAccount` and credentials.
struct Carol {
    oauth: Value,
    credentials: CredentialBlob,
}

impl Carol {
    fn new() -> Self {
        Self {
            oauth: oauth(CAROL, CAROL_ORG, CAROL_UUID),
            credentials: blob(CAROL_CREDENTIALS),
        }
    }

    fn at(&self, number: AccountNumber) -> NewAccount<'_> {
        account(number, CAROL, CAROL_ORG, &self.oauth, &self.credentials)
    }
}

/// Seals `text` as `credentials/<number>.cred` for `email` in `org` at `generation`, as a crashed add would leave it.
fn leftover(
    scratch: &Scratch,
    number: u32,
    org: &str,
    email: &str,
    generation: u64,
    text: &str,
) -> Vec<u8> {
    let sealed = seal::seal(
        SealPurpose::Credentials,
        &AssociatedData::credentials(number, org, generation, email),
        &blob(text),
    )
    .expect("a leftover seals");
    scratch.write_bytes(&format!("credentials/{number}.cred"), sealed.as_bytes());
    sealed.as_bytes().to_vec()
}

#[test]
fn add_accounts_writes_records_configs_and_sealed_credentials_into_an_empty_store() {
    let scratch = Scratch::empty("add_accounts_into_an_empty_store");
    let store = scratch.store();
    let lock = scratch.lock();
    let carol = Carol::new();
    let dave_oauth = oauth(DAVE, "", "");
    let dave_credentials = blob(
        r#"{"claudeAiOauth":{"accessToken":"fake-access-dave","refreshToken":"fake-refresh-dave"}}"#,
    );

    let numbers = store
        .add_accounts(
            &lock,
            &[
                carol.at(AccountNumber::Next),
                account(
                    AccountNumber::Next,
                    DAVE,
                    "",
                    &dave_oauth,
                    &dave_credentials,
                ),
            ],
        )
        .expect("two accounts are added to an empty store");

    assert_eq!(numbers, vec![1, 2]);
    let written = scratch.read_json("accounts.json");
    assert_eq!(written["order"], json!([1, 2]));
    assert_eq!(written["nextNumber"], json!(3));
    assert_eq!(written["schemaVersion"], json!(1));
    assert_eq!(
        written["accounts"]["1"],
        json!({
            "email": CAROL,
            "uuid": CAROL_UUID,
            "orgUuid": CAROL_ORG,
            "orgName": "",
            "added": "2026-10-23T10:00:00Z",
            "generation": 1,
        })
    );
    assert_eq!(written["accounts"]["2"]["email"], json!(DAVE));
    assert_eq!(
        written.get("active"),
        None,
        "the caller sets the active account"
    );
    assert_eq!(
        scratch.read_json("configs/1.json"),
        json!({"oauthAccount": carol.oauth}),
        "the config holds the identity only"
    );
    assert_eq!(
        scratch.read_json("configs/2.json"),
        json!({"oauthAccount": dave_oauth})
    );
    let opened = store
        .read_credentials(1)
        .expect("carol's file opens")
        .expect("carol has credentials");
    assert_eq!(opened.expose(), CAROL_CREDENTIALS);
    let opened = store
        .read_credentials(2)
        .expect("dave's file opens")
        .expect("dave has credentials");
    assert_eq!(opened.expose(), dave_credentials.expose());
}

#[test]
fn add_accounts_next_takes_the_next_number_and_appends_to_order() {
    let scratch = Scratch::new("add_accounts_next_takes_the_next_number");
    let store = scratch.store();
    let lock = scratch.lock();
    let before = scratch.read_json("accounts.json");
    let carol = Carol::new();

    let numbers = store
        .add_accounts(&lock, &[carol.at(AccountNumber::Next)])
        .expect("carol is added");

    assert_eq!(numbers, vec![3]);
    let written = scratch.read_json("accounts.json");
    assert_eq!(written["order"], json!([1, 2, 3]));
    assert_eq!(written["nextNumber"], json!(4));
    assert_eq!(
        written["active"], before["active"],
        "the active account is left to the caller"
    );
    assert_eq!(
        written["futureTopLevel"], before["futureTopLevel"],
        "every other key is kept"
    );
    assert_eq!(
        written["accounts"]["2"], before["accounts"]["2"],
        "the other records are kept"
    );
    assert_eq!(written["accounts"]["1"]["alias"], json!("work"));
    let sequence = store
        .read_sequence()
        .expect("accounts.json reads")
        .expect("accounts.json exists");
    let record = sequence.account(3).expect("account 3 is recorded");
    assert_eq!(record.email.as_str(), CAROL);
    assert_eq!(record.generation, 1);
    assert!(!record.may_spend_credits);
}

#[test]
fn add_accounts_exactly_refuses_a_number_below_next_or_already_recorded() {
    let scratch = Scratch::new("add_accounts_exactly_refuses_a_taken_number");
    scratch.edit_json("accounts.json", |accounts| {
        accounts["nextNumber"] = json!(10);
    });
    let store = scratch.store();
    let lock = scratch.lock();
    let before = scratch.read("accounts.json");
    let carol = Carol::new();

    for number in [1, 2, 5, 9] {
        let error = store
            .add_accounts(&lock, &[carol.at(AccountNumber::Exactly(number))])
            .expect_err("a recorded number or one below the next is taken");
        assert_eq!(error.kind(), StoreErrorKind::NumberTaken, "number {number}");
        assert_eq!(error.account(), Some(number));
    }
    assert_eq!(scratch.read("accounts.json"), before, "nothing is written");
    assert!(!scratch.exists("configs/5.json"));

    let numbers = store
        .add_accounts(&lock, &[carol.at(AccountNumber::Exactly(12))])
        .expect("a number past the next is free");

    assert_eq!(numbers, vec![12]);
    let written = scratch.read_json("accounts.json");
    assert_eq!(written["order"], json!([1, 2, 12]));
    assert_eq!(written["nextNumber"], json!(13));
}

#[test]
fn add_accounts_exactly_keeps_batch_order_below_each_other() {
    let scratch = Scratch::empty("add_accounts_exactly_keeps_batch_order");
    let store = scratch.store();
    let lock = scratch.lock();
    let carol = Carol::new();
    let dave_oauth = oauth(DAVE, "", "");
    let erin_oauth = oauth("erin@example.com", "", "");
    let credentials = blob(CAROL_CREDENTIALS);

    let numbers = store
        .add_accounts(
            &lock,
            &[
                carol.at(AccountNumber::Exactly(3)),
                account(AccountNumber::Exactly(1), DAVE, "", &dave_oauth, &credentials),
                account(AccountNumber::Exactly(2), "erin@example.com", "", &erin_oauth, &credentials),
            ],
        )
        .expect("numbers at or past the stored next number are free, whatever the batch took before them");

    assert_eq!(numbers, vec![3, 1, 2]);
    let written = scratch.read_json("accounts.json");
    assert_eq!(
        written["order"],
        json!([3, 1, 2]),
        "order keeps the batch's order"
    );
    assert_eq!(
        written["nextNumber"],
        json!(4),
        "one past the highest number"
    );
}

#[test]
fn add_accounts_refuses_u32_max_and_a_stored_next_number_of_u32_max() {
    let scratch = Scratch::new("add_accounts_refuses_u32_max");
    let store = scratch.store();
    let lock = scratch.lock();
    let carol = Carol::new();

    let error = store
        .add_accounts(&lock, &[carol.at(AccountNumber::Exactly(u32::MAX))])
        .expect_err("u32::MAX is no account number");
    assert_eq!(error.kind(), StoreErrorKind::NumbersExhausted);

    scratch.edit_json("accounts.json", |accounts| {
        accounts["nextNumber"] = json!(u32::MAX);
    });
    let before = scratch.read("accounts.json");
    let error = store
        .add_accounts(&lock, &[carol.at(AccountNumber::Next)])
        .expect_err("a next number of u32::MAX leaves none to give");

    assert_eq!(error.kind(), StoreErrorKind::NumbersExhausted);
    assert_eq!(scratch.read("accounts.json"), before, "nothing is written");
}

#[test]
fn add_accounts_refuses_an_email_and_org_already_stored_or_twice_in_one_batch() {
    let scratch = Scratch::new("add_accounts_refuses_an_existing_identity");
    let store = scratch.store();
    let lock = scratch.lock();
    let before = scratch.read("accounts.json");
    let credentials = blob(CAROL_CREDENTIALS);
    let alice = oauth(ALICE, ALICE_ORG, "");
    let bob = oauth(BOB, "", "");
    let carol = Carol::new();

    let stored_oauth = store
        .add_accounts(
            &lock,
            &[account(
                AccountNumber::Next,
                ALICE,
                ALICE_ORG,
                &alice,
                &credentials,
            )],
        )
        .expect_err("alice is stored");
    let stored_api_key = store
        .add_accounts(
            &lock,
            &[account(AccountNumber::Next, BOB, "", &bob, &credentials)],
        )
        .expect_err("bob is stored as an API-key account");
    let twice = store
        .add_accounts(
            &lock,
            &[carol.at(AccountNumber::Next), carol.at(AccountNumber::Next)],
        )
        .expect_err("carol cannot be added twice in one batch");

    assert_eq!(stored_oauth.kind(), StoreErrorKind::AccountExists);
    assert_eq!(stored_oauth.account(), Some(1));
    assert_eq!(stored_api_key.kind(), StoreErrorKind::AccountExists);
    assert_eq!(stored_api_key.account(), Some(2));
    assert_eq!(twice.kind(), StoreErrorKind::AccountExists);
    assert_eq!(scratch.read("accounts.json"), before, "nothing is written");
    assert!(
        !scratch.exists("configs/3.json"),
        "no config is written before the checks pass"
    );
    assert!(
        !scratch.exists("credentials/3.cred"),
        "no credential is written before the checks pass"
    );
}

#[test]
fn add_accounts_next_skips_a_number_whose_leftover_does_not_open() {
    let scratch = Scratch::new("add_accounts_next_skips_an_unopenable_leftover");
    let store = scratch.store();
    let lock = scratch.lock();
    let foreign = leftover(&scratch, 3, "", DAVE, 1, CAROL_CREDENTIALS);
    let carol = Carol::new();

    let numbers = store
        .add_accounts(&lock, &[carol.at(AccountNumber::Next)])
        .expect("carol is added past it");

    assert_eq!(numbers, vec![4]);
    assert_eq!(
        scratch.read("credentials/3.cred"),
        foreign,
        "the leftover is kept as it was"
    );
    let written = scratch.read_json("accounts.json");
    assert_eq!(written["order"], json!([1, 2, 4]));
    assert_eq!(
        written["nextNumber"],
        json!(5),
        "the skipped number is not given later"
    );
}

#[test]
fn add_accounts_exactly_refuses_an_unopenable_leftover() {
    let scratch = Scratch::new("add_accounts_exactly_refuses_an_unopenable_leftover");
    let store = scratch.store();
    let lock = scratch.lock();
    let before = scratch.read("accounts.json");
    let foreign = leftover(&scratch, 3, "", DAVE, 1, CAROL_CREDENTIALS);
    let carol = Carol::new();

    let error = store
        .add_accounts(&lock, &[carol.at(AccountNumber::Exactly(3))])
        .expect_err("another login's leftover is not sealed over");

    assert_eq!(error.kind(), StoreErrorKind::LeftoverUnopenable);
    assert_eq!(error.account(), Some(3));
    assert_eq!(scratch.read("credentials/3.cred"), foreign);
    assert_eq!(scratch.read("accounts.json"), before, "nothing is written");

    // Carol's own leftover, from a crashed earlier add, opens: it is sealed over at the next generation and kept.
    let own = leftover(
        &scratch,
        3,
        CAROL_ORG,
        CAROL,
        4,
        r#"{"claudeAiOauth":{"accessToken":"fake-access-old"}}"#,
    );
    let numbers = store
        .add_accounts(&lock, &[carol.at(AccountNumber::Exactly(3))])
        .expect("carol's own leftover is sealed over");

    assert_eq!(numbers, vec![3]);
    assert_eq!(
        scratch.read_json("accounts.json")["accounts"]["3"]["generation"],
        json!(5)
    );
    assert_eq!(
        scratch.read("credentials/3.cred.prev"),
        own,
        "the leftover is kept as .prev"
    );
    let opened = store
        .read_credentials(3)
        .expect("carol's file opens")
        .expect("carol has credentials");
    assert_eq!(opened.expose(), CAROL_CREDENTIALS);
}

#[test]
fn add_accounts_writes_nothing_under_a_write_refusal() {
    let scratch = Scratch::new("add_accounts_writes_nothing_under_a_write_refusal");
    let store = scratch.store_without_sealing();
    let lock = scratch.lock();
    let before = scratch.read("accounts.json");
    let carol = Carol::new();

    let error = store
        .add_accounts(&lock, &[carol.at(AccountNumber::Next)])
        .expect_err("a store that cannot seal takes no account");

    assert_eq!(error.kind(), StoreErrorKind::StoreReadOnly);
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "accounts.json is unchanged"
    );
    assert!(!scratch.exists("configs/3.json"), "no config is created");
    assert!(
        !scratch.exists("credentials/3.cred"),
        "no credential file is created"
    );
}
