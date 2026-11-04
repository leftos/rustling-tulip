//! `UsageTable::entries`: the store's `cache/usage.json` at schema 3, read without a lock and guarded by identity.

use std::collections::BTreeMap;

use claude_accounts::{Identity, UsageEntry, UsageTable};
use serde_json::json;

use crate::support::{FETCHED_AT, ONE, ONE_ORG, Scratch, TWO, identities};

fn timestamp(text: &str) -> jiff::Timestamp {
    text.parse().expect("a test timestamp parses")
}

#[test]
fn entries_reads_schema_three() {
    let scratch = Scratch::new("entries_reads_schema_three");

    let entries = UsageTable::entries(&scratch.root(), &identities());

    assert_eq!(entries.len(), 2);
    let one = &entries[&1];
    assert_eq!(one.identity, Some(Identity::new(ONE, ONE_ORG)));
    let five_hour = one.five_hour.as_ref().expect("account 1 has a 5 h window");
    assert!((five_hour.pct - 25.0).abs() < f64::EPSILON);
    assert_eq!(five_hour.resets_at, Some(timestamp("2026-09-21T17:00:00Z")));
    let seven_day = one.seven_day.as_ref().expect("account 1 has a 7 d window");
    assert!((seven_day.pct - 10.0).abs() < f64::EPSILON);
    assert_eq!(seven_day.resets_at, Some(timestamp("2026-09-25T09:00:00Z")));
    assert_eq!(
        one.fetched_at.map(jiff::Timestamp::as_second),
        Some(1_790_000_000)
    );
    assert_eq!(
        one.next_poll_at.map(jiff::Timestamp::as_second),
        Some(1_790_000_300)
    );
    assert_eq!(one.poll_interval_s, Some(300.0));
    assert_eq!(one.consecutive_failures, 0);
    assert_eq!(one.claim_id, None);
    let last_good = one
        .last_good
        .as_ref()
        .expect("account 1 keeps its raw last-good reading");
    assert_eq!(
        last_good["scoped"][0]["name"],
        json!("Fable"),
        "the per-model windows stay in the raw reading"
    );
    assert_eq!(one.extra.get("futureRowKey"), Some(&json!({"kept": true})));
    assert!(
        !one.extra.contains_key("email"),
        "modelled keys are not repeated in the extra map"
    );

    let two = &entries[&2];
    let five_hour = two.five_hour.as_ref().expect("account 2 has a 5 h window");
    assert!(
        (five_hour.pct - 80.0).abs() < f64::EPSILON,
        "an integer percentage reads as a number"
    );
    assert_eq!(five_hour.resets_at, None);
    let seven_day = two.seven_day.as_ref().expect("account 2 has a 7 d window");
    assert!((seven_day.pct - 55.5).abs() < f64::EPSILON);
    assert_eq!(two.consecutive_failures, 1);
    assert_eq!(two.last_error.as_deref(), Some("http-429"));
    assert_eq!(
        two.backoff_until.map(jiff::Timestamp::as_second),
        Some(1_790_000_020)
    );
    assert_eq!(
        two.last_429_at.map(jiff::Timestamp::as_second),
        Some(1_789_999_900)
    );
    assert_eq!(
        two.fetched_at.map(jiff::Timestamp::as_second),
        Some(1_789_996_000)
    );
}

#[test]
fn mismatched_identity_gives_empty_entry() {
    let scratch = Scratch::new("mismatched_identity_gives_empty_entry");
    let identities = BTreeMap::from([
        // Slot 1 now holds another account; slot 2 the same email under an organization the row does not name.
        (1, Identity::new("someone-else@example.test", ONE_ORG)),
        (
            2,
            Identity::new(TWO, "00000000-0000-4000-8000-0000000000c2"),
        ),
        // A slot with no row at all.
        (3, Identity::new("three@example.test", "")),
    ]);

    let entries = UsageTable::entries(&scratch.root(), &identities);

    assert_eq!(entries.len(), 3);
    for number in [1, 2, 3] {
        assert_eq!(
            entries[&number],
            UsageEntry::default(),
            "account {number} reads as empty"
        );
    }
}

#[test]
fn every_unknown_version_reads_as_empty() {
    let scratch = Scratch::new("every_unknown_version_reads_as_empty");
    let row = json!({"email": ONE, "organizationUuid": ONE_ORG, "lastGood": {"five_hour": {"pct": 25.0}}, "fetchedAt": FETCHED_AT});
    for document in [
        json!({"schemaVersion": 1, "accounts": {"1": row}}),
        json!({"timestamp": FETCHED_AT, "data": {"1": {"five_hour": {"pct": 25.0}}}}),
        json!({"schemaVersion": 2, "accounts": {"1": row}}),
        json!({"schemaVersion": 3.0, "accounts": {"1": row}}),
        json!({"schemaVersion": "3", "accounts": {"1": row}}),
        json!({"schemaVersion": 4, "accounts": {"1": row}}),
    ] {
        scratch.write("cache/usage.json", &document.to_string());

        let entries = UsageTable::entries(&scratch.root(), &identities());

        assert_eq!(
            entries[&1],
            UsageEntry::default(),
            "a file whose schema is not the integer 3 reads as empty"
        );
        assert_eq!(entries[&2], UsageEntry::default());
    }
    scratch.write("cache/usage.json", "{not json");
    assert_eq!(
        UsageTable::entries(&scratch.root(), &identities())[&1],
        UsageEntry::default()
    );
}
