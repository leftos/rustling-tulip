//! An account's alias: set trimmed, removed when none or blank, refused when too long or holding a control character,
//! under the account lock.

use claude_accounts::StoreErrorKind;

use crate::support::Scratch;

/// Account `number`'s record keys in `accounts.json`, in file order.
fn record_keys(scratch: &Scratch, number: &str) -> Vec<String> {
    scratch.read_json("accounts.json")["accounts"][number]
        .as_object()
        .expect("the record is an object")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn set_alias_sets_and_trims() {
    let scratch = Scratch::new("set_alias_sets_and_trims");
    let store = scratch.store();
    let guard = scratch.lock();
    let keys = record_keys(&scratch, "1");

    assert!(
        store
            .set_alias(&guard, 1, Some("  personal  "))
            .expect("the alias is set")
    );

    let written = scratch.read_json("accounts.json");
    assert_eq!(written["accounts"]["1"]["alias"], "personal");
    assert_eq!(
        record_keys(&scratch, "1"),
        keys,
        "the alias keeps its place"
    );
    assert_eq!(written["futureTopLevel"], "kept");

    assert!(
        store
            .set_alias(&guard, 2, Some("api"))
            .expect("an account without an alias gets one")
    );
    assert_eq!(
        scratch.read_json("accounts.json")["accounts"]["2"]["alias"],
        "api"
    );

    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert_eq!(
        sequence
            .account(1)
            .expect("account 1 is kept")
            .alias
            .as_deref(),
        Some("personal")
    );
}

#[test]
fn set_alias_none_or_blank_removes_it() {
    let scratch = Scratch::new("set_alias_none_or_blank_removes_it");
    let store = scratch.store();
    let guard = scratch.lock();

    assert!(
        store
            .set_alias(&guard, 1, Some("   "))
            .expect("a blank alias removes it")
    );
    assert!(
        !record_keys(&scratch, "1").iter().any(|key| key == "alias"),
        "the alias key is gone"
    );

    assert!(store.set_alias(&guard, 1, Some("home")).expect("set again"));
    assert!(store.set_alias(&guard, 1, None).expect("none removes it"));
    assert!(!record_keys(&scratch, "1").iter().any(|key| key == "alias"));
    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert_eq!(sequence.account(1).expect("account 1 is kept").alias, None);
}

#[test]
fn set_alias_unchanged_returns_false() {
    let scratch = Scratch::new("set_alias_unchanged_returns_false");
    let store = scratch.store();
    let guard = scratch.lock();
    let before = scratch.read("accounts.json");

    assert!(
        !store
            .set_alias(&guard, 1, Some("work"))
            .expect("the same alias is fine")
    );
    assert!(
        !store
            .set_alias(&guard, 1, Some(" work "))
            .expect("the same alias once trimmed is fine")
    );
    assert!(
        !store
            .set_alias(&guard, 2, None)
            .expect("no alias on an account without one is fine")
    );
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "the file is untouched, byte for byte"
    );
}

#[test]
fn set_alias_refuses_over_64_chars() {
    let scratch = Scratch::new("set_alias_refuses_over_64_chars");
    let store = scratch.store();
    let guard = scratch.lock();
    let before = scratch.read("accounts.json");

    let error = store
        .set_alias(&guard, 1, Some(&"a".repeat(65)))
        .expect_err("65 characters are too many");
    assert_eq!(error.kind(), StoreErrorKind::InvalidAlias);
    assert_eq!(error.account(), Some(1));
    assert_eq!(scratch.read("accounts.json"), before);

    let longest = "\u{e9}".repeat(64);
    assert!(
        store
            .set_alias(&guard, 1, Some(&format!(" {longest} ")))
            .expect("64 characters, counted after trimming, are allowed")
    );
    assert_eq!(
        scratch.read_json("accounts.json")["accounts"]["1"]["alias"],
        longest.as_str()
    );
}

#[test]
fn set_alias_refuses_control_characters() {
    let scratch = Scratch::new("set_alias_refuses_control_characters");
    let store = scratch.store();
    let guard = scratch.lock();
    let before = scratch.read("accounts.json");

    for alias in ["wo\u{7}rk", "a\nb", "esc\u{1b}[31m", "del\u{7f}"] {
        let error = store
            .set_alias(&guard, 1, Some(alias))
            .expect_err("a control character is refused");
        assert_eq!(error.kind(), StoreErrorKind::InvalidAlias, "{alias:?}");
        assert_eq!(error.account(), Some(1));
    }
    assert_eq!(scratch.read("accounts.json"), before);
}

#[test]
fn set_alias_refuses_bidi_and_invisible_characters() {
    let scratch = Scratch::new("set_alias_refuses_bidi_and_invisible_characters");
    let store = scratch.store();
    let guard = scratch.lock();
    let before = scratch.read("accounts.json");

    for alias in [
        "work\u{202E}fdp.exe",
        "a\u{2066}b\u{2069}",
        "zero\u{200B}width",
        "line\u{2028}break",
        "para\u{2029}graph",
        "mark\u{200F}",
        "\u{FEFF}bom",
    ] {
        let error = store
            .set_alias(&guard, 1, Some(alias))
            .expect_err("a bidi or invisible character is refused");
        assert_eq!(error.kind(), StoreErrorKind::InvalidAlias, "{alias:?}");
        assert_eq!(error.account(), Some(1));
    }
    assert_eq!(scratch.read("accounts.json"), before);
}
