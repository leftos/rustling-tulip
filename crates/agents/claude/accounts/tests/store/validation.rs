//! An account email that could leave its folder as part of a file name is refused, as is a non-canonical key.

use claude_accounts::{Email, StoreErrorKind};

use crate::support::Scratch;

fn roster(key: &str, email_json: &str) -> String {
    format!(
        r#"{{"active": 3, "order": [3], "accounts": {{"{key}": {{"email": {email_json}, "uuid": "u"}}}}}}"#
    )
}

#[test]
fn unsafe_email_is_refused() {
    let scratch = Scratch::new("unsafe_email_is_refused");
    let store = scratch.store();
    let cases = [
        r#""../../../evil""#,
        r#""..\\..\\evil""#,
        r#""a/evil@example.com""#,
        r#""c:evil@example.com""#,
        r#""tab\u0009evil@example.com""#,
        r#""nul\u0000evil@example.com""#,
        r#""""#,
    ];
    for email in cases {
        scratch.write("accounts.json", &roster("3", email));
        let error = store
            .read_sequence()
            .expect_err("an unsafe email refuses the roster");
        assert_eq!(error.kind(), StoreErrorKind::UnsafeEmail, "{email}");
        assert_eq!(error.account(), Some(3), "{email}");
        assert!(
            !error.to_string().contains("evil"),
            "the error never repeats the email: {error}"
        );
    }

    let guard = scratch.lock();
    let before = scratch.read("accounts.json");
    let error = store
        .set_disabled(&guard, 3, true)
        .expect_err("a roster with an unsafe email is never rewritten");
    assert_eq!(error.kind(), StoreErrorKind::UnsafeEmail);
    assert_eq!(scratch.read("accounts.json"), before);
}

#[test]
fn email_newtype_rejects_traversal() {
    let cases = [
        "../../../x",
        "..\\..\\x",
        "a/b@example.com",
        "a\\b@example.com",
        "c:x@example.com",
        "a*b",
        "a?b",
        "a\"b",
        "a<b",
        "a>b",
        "a|b",
        "a\nb",
        "a\0b",
        "a\u{1f}b",
        "",
    ];
    for text in cases {
        let error = Email::parse(text).expect_err("an unsafe email is refused");
        assert_eq!(error.kind(), StoreErrorKind::UnsafeEmail, "{text:?}");
        assert_eq!(error.account(), None);
    }

    for text in ["first.last+tag@example.com", "..", "x..y@example.com"] {
        let email = Email::parse(text).expect("a file-name-safe email is kept");
        assert_eq!(email.as_str(), text);
    }
}

#[test]
fn non_canonical_account_key_is_corrupt() {
    let scratch = Scratch::new("non_canonical_account_key_is_corrupt");
    let store = scratch.store();
    for key in ["03", "+3", " 3", "3.0"] {
        scratch.write("accounts.json", &roster(key, r#""carol@example.com""#));
        let error = store
            .read_sequence()
            .expect_err("a non-canonical key is not a roster");
        assert_eq!(error.kind(), StoreErrorKind::AccountsCorrupt, "{key:?}");
    }
    scratch.write("accounts.json", &roster("3", r#""carol@example.com""#));
    let sequence = store
        .read_sequence()
        .expect("a canonical key reads")
        .expect("the roster exists");
    assert_eq!(
        sequence
            .account(3)
            .expect("account 3 is read")
            .email
            .as_str(),
        "carol@example.com"
    );
}
