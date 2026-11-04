//! The usage reading a switch decision trusts, read from a usage table as cswap writes it.

use std::collections::BTreeMap;

use claude_accounts::engine::parse_model_names;
use claude_accounts::{AccountsRoot, Identity, UsageEntry, UsageTable, decision_value};
use serde_json::json;

use crate::support::{NOW_S, Scratch, at, write};

const EMAIL: &str = "one@example.test";

/// Account 1's entry: a reading fetched 1000 s ago and frozen by a 429, whose `Fable` window reset an hour ago.
fn rate_limited_entry(root: &AccountsRoot) -> UsageEntry {
    let table = json!({
        "schemaVersion": 3,
        "accounts": {
            "1": {
                "email": EMAIL,
                "organizationUuid": "",
                "lastGood": {
                    "five_hour": {"pct": 40.0},
                    "seven_day": {"pct": 60.0},
                    "scoped": [{"name": "Fable", "pct": 90.0, "resets_at": "2026-09-21T13:13:20Z"}]
                },
                "fetchedAt": NOW_S - 1000,
                "consecutiveFailures": 1,
                "lastError": "http-429"
            }
        }
    });
    write(&root.cache_dir().join("usage.json"), &table.to_string());
    let identities = BTreeMap::from([(1, Identity::new(EMAIL, ""))]);
    UsageTable::entries(root, &identities)
        .remove(&1)
        .expect("account 1 has an entry")
}

#[test]
fn decision_value_matches_models_case_insensitively() {
    let scratch = Scratch::new("decision_value_matches_models_case_insensitively");
    let root = scratch.root();
    std::fs::create_dir_all(root.cache_dir()).expect("the cache folder can be made");
    let entry = rate_limited_entry(&root);
    let models = |names: &[&str]| {
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    };

    assert!(
        decision_value(&entry, &models(&["opus"]), at(0)).is_some(),
        "with no named window reset, the 429-frozen reading is trusted"
    );
    for named in [&["fable"][..], &["Fable"], &["FABLE"], &["All"]] {
        assert!(
            decision_value(&entry, &models(named), at(0)).is_none(),
            "{named:?} names the Fable window, whose reset has passed"
        );
    }
}

// cswap: settings.parse_model_names
#[test]
fn model_names_dedupe_case_insensitively_first_spelling_wins() {
    assert_eq!(
        parse_model_names(Some(" Fable, fable ,Opus,,")),
        ["Fable", "Opus"]
    );
    assert!(
        parse_model_names(Some(" , ,")).is_empty(),
        "only empty names"
    );
    assert!(parse_model_names(None).is_empty(), "no setting");
}

// cswap: settings.parse_model_names keeps `all` as a name; the matching gives it its meaning.
#[test]
fn model_names_keep_all_sentinel() {
    assert_eq!(parse_model_names(Some("all")), ["all"]);
    assert_eq!(parse_model_names(Some("Fable, ALL, all")), ["Fable", "ALL"]);
}
