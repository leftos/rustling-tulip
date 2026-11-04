//! Every lock file in the account store's root verifies the root before it is taken: a fresh machine's first lock makes
//! the root owner-only, and a root another principal can write, or one reached through a junction, is refused with
//! nothing changed under it.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use claude_accounts::{
    AccountLock, AccountsRoot, AutoswitchState, ConsumeLock, Identity, Lease, LeaseHolder, Store,
    USAGE_SCHEMA_VERSION, UsageEntry, UsageTable,
};
use proc_table::EmptyTable;

use crate::links::junction;
use crate::support::{ALICE, Scratch, assert_owner_only, grant_users_write};

/// Every file and folder under `folder`, by its path relative to it, with each file's bytes (a folder's are empty).
fn tree(folder: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut listing = BTreeMap::new();
    collect(folder, folder, &mut listing);
    listing
}

fn collect(base: &Path, folder: &Path, listing: &mut BTreeMap<String, Option<Vec<u8>>>) {
    for entry in fs::read_dir(folder).expect("the folder lists") {
        let path = entry.expect("an entry can be read").path();
        let name = path
            .strip_prefix(base)
            .expect("the entry is under the folder")
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            listing.insert(name, None);
            collect(base, &path, listing);
        } else {
            listing.insert(
                name,
                Some(fs::read(&path).expect("a file under the folder can be read")),
            );
        }
    }
}

fn me() -> LeaseHolder {
    LeaseHolder::new("rustling-tulip", std::process::id(), 30.0)
}

/// Account 1, ALICE, as the usage table names her.
fn identities() -> BTreeMap<u32, Identity> {
    BTreeMap::from([(
        1,
        Identity::new(ALICE, "00000000-0000-4000-8000-0000000000a1"),
    )])
}

/// Writes a usage table of this store's version with a reading for ALICE in account 1's row.
fn write_alice_usage(scratch: &Scratch) {
    let table = serde_json::json!({
        "schemaVersion": USAGE_SCHEMA_VERSION,
        "accounts": {"1": {
            "email": ALICE,
            "organizationUuid": "00000000-0000-4000-8000-0000000000a1",
            "lastGood": {"five_hour": {"pct": 25.0}},
            "fetchedAt": 1_790_000_000.0,
        }},
    });
    scratch.write("cache/usage.json", &table.to_string());
}

/// The usage reserve, the state read, the lease claim, the account lock and the consume lock each fail on `root`, and
/// the lock-free usage read gives ALICE an empty entry.
fn assert_every_root_lock_refuses(root: &AccountsRoot) {
    let reserved = UsageTable::reserve(root, &identities(), jiff::Timestamp::now());
    assert!(
        reserved.is_err(),
        "the usage reserve is refused: {reserved:?}"
    );
    let state = AutoswitchState::read(root);
    assert!(state.is_err(), "the state read is refused");
    let claimed = Lease::new(root).claim_or_heartbeat(&me(), jiff::Timestamp::now(), &EmptyTable);
    assert!(claimed.is_err(), "the lease claim is refused: {claimed:?}");
    assert!(
        AccountLock::acquire(root).is_err(),
        "the account lock is refused"
    );
    assert!(
        ConsumeLock::acquire(root, 1).is_err(),
        "the consume lock is refused"
    );
    assert_eq!(
        UsageTable::entries(root, &identities())[&1],
        UsageEntry::default(),
        "the usage table is not read through a refused root"
    );
}

/// The engine's lease claim, the first thing to touch the store on a fresh machine, makes the root and its parent
/// owner-only rather than letting them inherit the DACL of the folder they were made in.
#[test]
fn a_lease_claim_first_on_a_fresh_machine_makes_the_root_owner_only() {
    let scratch =
        Scratch::empty("a_lease_claim_first_on_a_fresh_machine_makes_the_root_owner_only");
    let parent = scratch.dir.join("rustling-tulip");
    let root = AccountsRoot::new(parent.join("accounts"));

    Lease::new(&root)
        .claim_or_heartbeat(&me(), jiff::Timestamp::now(), &EmptyTable)
        .expect("the lease is claimed");

    assert_owner_only(&parent);
    assert_owner_only(root.root());
}

/// A missing root under a parent that already exists and is owner-only is made owner-only by the first lock, and the
/// store opens it: the parent's strict rule is met, and nothing about the parent changes.
#[test]
fn missing_root_is_created_under_an_owner_only_parent() {
    let scratch = Scratch::empty("missing_root_is_created_under_an_owner_only_parent");
    let root = scratch.root();
    assert!(!root.root().exists(), "the root starts missing");
    assert_owner_only(&scratch.dir);

    drop(AccountLock::acquire(&root).expect("the first lock makes the root"));

    assert!(root.root().is_dir(), "the root is made");
    assert_owner_only(root.root());
    assert_owner_only(&scratch.dir);
    let store = Store::open(root).expect("the store opens over the new root");
    assert!(
        store.read_sequence().expect("the new root reads").is_none(),
        "a new root holds no accounts"
    );
}

/// A root another principal can write is refused by every lock in it, and nothing under it changes.
#[test]
fn every_root_lock_refuses_a_loose_root() {
    let scratch = Scratch::new("every_root_lock_refuses_a_loose_root");
    write_alice_usage(&scratch);
    assert_ne!(
        UsageTable::entries(&scratch.root(), &identities())[&1],
        UsageEntry::default(),
        "ALICE's row reads through a good root"
    );
    grant_users_write(&scratch.backup());
    let before = tree(&scratch.backup());

    assert_every_root_lock_refuses(&scratch.root());

    assert_eq!(
        tree(&scratch.backup()),
        before,
        "nothing under the root changed"
    );
}

/// A root that is a junction to another folder is refused by every lock in it, and the folder behind it is unchanged.
#[test]
fn every_root_lock_refuses_a_junctioned_root() {
    let scratch = Scratch::new("every_root_lock_refuses_a_junctioned_root");
    write_alice_usage(&scratch);
    let real = scratch.dir.join("real-backup");
    fs::rename(scratch.backup(), &real).expect("the account store can be moved aside");
    junction(&scratch.backup(), &real);
    let before = tree(&real);

    assert_every_root_lock_refuses(&scratch.root());

    assert_eq!(
        tree(&real),
        before,
        "the folder behind the junction is unchanged"
    );
}
