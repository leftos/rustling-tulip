//! The consume gate: the order it refuses, defers and spends in, against a scratch account store.

use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use claude_accounts::{ConsumeLock, CredentialBlob, GrantError, Store, credential_fingerprint};
use serde_json::{Value, json};

use crate::support::{
    FakeEndpoint, ONE, ONE_ORG, Rig, Scratch, access_of, answer, credential, granted, ms_after,
};

fn expired() -> String {
    credential("fake-access-one", Some("fake-refresh-one"), ms_after(-3600))
}

fn fingerprint(text: &str) -> Option<String> {
    credential_fingerprint(&CredentialBlob::new(text.to_owned()))
}

/// A scratch root whose account 1 is inactive and stores [`expired`].
fn inactive_one(test: &str) -> Scratch {
    let scratch = Scratch::new(test);
    scratch.set_active(2);
    crate::support::put_credentials(&scratch.root(), &expired());
    scratch
}

/// A rig whose inactive account 1 stores [`expired`] and whose endpoint grants `fake-access-new` once.
fn rig(test: &str) -> Rig {
    let scratch = inactive_one(test);
    Rig::new(
        scratch,
        FakeEndpoint::answering(vec![granted(
            "fake-access-new",
            Some("fake-refresh-new"),
            3600,
        )]),
    )
}

#[test]
fn securestorage_dir_set_refuses_with_store_unmirrored_before_any_lock() {
    let mut rig = rig("store_unmirrored");
    rig.securestorage_dir_set = true;
    let _held = ConsumeLock::acquire(&rig.scratch.root(), 1).expect("the consume lock is free");

    let started = Instant::now();
    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, Some(GrantError::StoreUnmirrored));
    assert_eq!(GrantError::StoreUnmirrored.kind(), "store-unmirrored");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the refusal waited on the held consume lock"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
}

/// Waits out the consume lock's 10 s timeout: the lock takes no shorter timing.
#[test]
fn held_consume_lock_defers_with_consume_busy_and_posts_nothing() {
    let rig = rig("consume_busy");
    let _held = ConsumeLock::acquire(&rig.scratch.root(), 1).expect("the consume lock is free");

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, Some(GrantError::ConsumeBusy));
    assert_eq!(GrantError::ConsumeBusy.kind(), "consume-busy");
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "the store is untouched"
    );
}

#[test]
fn refresh_after_drain_posts_nothing() {
    let rig = rig("after_drain");
    assert_eq!(rig.drain.start(), 0);

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, Some(GrantError::Draining));
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "the store is untouched"
    );
}

#[test]
fn a_failing_canary_before_the_inactive_spend_posts_nothing() {
    let mut rig = rig("inactive_canary_failing");
    rig.store =
        Store::open_with_canary(rig.scratch.root(), || false).expect("the scratch store opens");

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error,
        Some(GrantError::StoreUnwritable),
        "the refused spend is deferred, not spent"
    );
    assert_eq!(GrantError::StoreUnwritable.kind(), "store-unwritable");
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "the store is untouched"
    );
}

/// The canary is checked before the drain guard: with the drain already started, the store's refusal is still the one
/// reported, where a check after `drain.begin` would report [`GrantError::Draining`] instead.
#[test]
fn a_failing_canary_is_refused_before_the_drain_guard_is_taken() {
    let mut rig = rig("inactive_canary_before_drain");
    rig.store =
        Store::open_with_canary(rig.scratch.root(), || false).expect("the scratch store opens");
    assert_eq!(rig.drain.start(), 0, "the drain starts with no guard held");

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error,
        Some(GrantError::StoreUnwritable),
        "the store's refusal comes before the drain's"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
}

/// An `accounts.json` of a later schema refuses the inactive spend as the failing canary does: nothing is posted.
#[test]
fn an_unknown_schema_accounts_json_spends_no_grant() {
    let rig = rig("inactive_later_schema");
    let path = rig.scratch.backup().join("accounts.json");
    let mut accounts: Value =
        serde_json::from_slice(&fs::read(&path).expect("accounts.json can be read"))
            .expect("it is JSON");
    accounts["schemaVersion"] = json!(2);
    fs::write(&path, accounts.to_string()).expect("accounts.json can be written");

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error,
        Some(GrantError::StoreUnwritable),
        "the refused spend is deferred, not spent"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "the store is untouched"
    );
}

#[test]
fn a_passing_canary_still_spends_the_inactive_grant_once() {
    let rig = rig("inactive_canary_passing");

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, None);
    assert_eq!(rig.endpoint.bodies().len(), 1, "the grant is posted once");
    assert_eq!(
        access_of(&rig.stored().expect("the slot holds a credential")),
        "fake-access-new"
    );
}

#[test]
fn inactive_refresh_drain_begun_during_lock_wait_spends_nothing() {
    let rig = rig("drain_during_lock_wait");
    let held = ConsumeLock::acquire(&rig.scratch.root(), 1).expect("the consume lock is free");

    let outcome = thread::scope(|scope| {
        let worker = scope.spawn(|| rig.consume(&expired()));
        thread::sleep(Duration::from_millis(500));
        let _in_flight = rig.drain.start();
        drop(held);
        worker.join().expect("the consume finishes")
    });

    assert_eq!(outcome.error, Some(GrantError::Draining));
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "the store is untouched"
    );
}

#[test]
fn drain_waits_for_a_refresh_in_flight() {
    let scratch = inactive_one("drain_waits");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )])
    .during_post(move || {
        entered_tx.send(()).expect("the test waits for the POST");
        release_rx.recv().expect("the test releases the POST");
    });
    let rig = Rig::new(scratch, endpoint);

    thread::scope(|scope| {
        let worker = scope.spawn(|| rig.consume(&expired()));
        entered_rx.recv().expect("the POST starts");

        assert_eq!(rig.drain.start(), 1, "the refresh in flight holds a guard");
        assert!(
            !rig.drain.wait_idle(Duration::from_millis(200)),
            "the drain must wait for the POST"
        );
        release_tx.send(()).expect("the POST is waiting");
        assert!(
            rig.drain.wait_idle(Duration::from_secs(10)),
            "the guard drops once the refresh ends"
        );
        let stored = rig.stored().expect("the slot holds a credential");
        assert_eq!(
            access_of(&stored),
            "fake-access-new",
            "the successor was written before the guard dropped"
        );

        let outcome = worker.join().expect("the refresh thread finishes");
        assert_eq!(outcome.error, None);
    });
}

/// The first POST waits until the second caller is about to enter the gate, then a little longer, so the second gate
/// is waiting on the consume lock while the first grant is in flight.
#[test]
fn two_threads_refreshing_one_grant_post_once() {
    let scratch = inactive_one("two_threads");
    let (entering_tx, entering_rx) = mpsc::channel::<()>();
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )])
    .during_post(move || {
        entering_rx
            .recv()
            .expect("the second caller signals before it enters the gate");
        thread::sleep(Duration::from_millis(200));
    });
    let rig = Rig::new(scratch, endpoint);

    let outcomes = thread::scope(|scope| {
        let first = scope.spawn(|| rig.consume(&expired()));
        let second = scope.spawn(|| {
            entering_tx
                .send(())
                .expect("the first POST waits for the signal");
            rig.consume(&expired())
        });
        [
            first.join().expect("a refresh thread finishes"),
            second.join().expect("a refresh thread finishes"),
        ]
    });

    assert_eq!(rig.endpoint.bodies().len(), 1, "the grant was posted once");
    for outcome in &outcomes {
        assert_eq!(outcome.error, None);
        let credentials = outcome
            .credentials
            .as_ref()
            .expect("both callers get the current credential");
        assert_eq!(access_of(credentials.expose()), "fake-access-new");
    }
    assert_eq!(
        access_of(&rig.stored().expect("the slot holds a credential")),
        "fake-access-new"
    );
}

#[test]
fn removed_slot_never_spends() {
    let rig = rig("removed_slot");
    fs::remove_file(rig.scratch.credential_file()).expect("the credential file can be removed");

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, Some(GrantError::Transient));
    assert!(outcome.credentials.is_none());
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert!(!rig.scratch.manifest_path().exists(), "nothing was stashed");
}

#[test]
fn fresh_reread_token_skips_the_post() {
    let rig = rig("fresh_reread");
    let fresh = credential(
        "fake-access-fresh",
        Some("fake-refresh-fresh"),
        ms_after(3600),
    );
    crate::support::put_credentials(&rig.scratch.root(), &fresh);

    let outcome = rig.consume(&expired());

    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(outcome.error, None);
    let credentials = outcome
        .credentials
        .expect("the fresh credential is handed back");
    assert_eq!(credentials.expose(), fresh);
    assert_eq!(outcome.consumed_fp, fingerprint(&fresh));
}

/// The poll gathered account 1 as inactive; by the time its grant reaches the gate, it is the active account.
#[test]
fn account_that_became_active_before_the_gate_is_not_spent() {
    let rig = rig("became_active");
    rig.scratch.set_active(1);

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error,
        Some(GrantError::Active),
        "accounts.json's active account"
    );
    assert_eq!(GrantError::Active.kind(), "active");
    assert!(outcome.credentials.is_none());
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "the store is untouched"
    );

    rig.scratch.set_active(2);
    let identity = json!({ "oauthAccount": { "emailAddress": ONE, "organizationUuid": ONE_ORG } });
    fs::write(rig.scratch.dir.join(".claude.json"), identity.to_string())
        .expect("the global config can be written");

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error,
        Some(GrantError::Active),
        "the account Claude Code is logged in as"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "the store is untouched"
    );
}

/// The stored credential's OAuth block.
fn stored_oauth(rig: &Rig) -> Value {
    let stored: Value = serde_json::from_str(&rig.stored().expect("the slot holds a credential"))
        .expect("a credential is JSON");
    stored["claudeAiOauth"].clone()
}

/// The POST spent the old refresh token, so an answer carrying a new one is kept however degenerate the rest is.
#[test]
fn degenerate_answer_with_a_new_refresh_token_is_persisted() {
    let scratch = inactive_one("degenerate_answer");
    let degenerate = answer(
        200,
        json!({ "access_token": "", "refresh_token": "fake-refresh-new", "expires_in": 3600 }),
    );
    let rig = Rig::new(scratch, FakeEndpoint::answering(vec![degenerate]));

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, None);
    assert_eq!(
        outcome.stash, None,
        "the successor was written, not stashed"
    );
    let oauth = stored_oauth(&rig);
    assert_eq!(
        oauth["refreshToken"], "fake-refresh-new",
        "the new refresh token is the live one"
    );
    assert_eq!(oauth["accessToken"], "", "no access token was granted");
    assert_eq!(
        oauth["expiresAt"],
        ms_after(0),
        "it reads as expired at the refresh's clock, so the next pass refreshes it"
    );
}

#[test]
fn short_expires_in_is_persisted_and_refreshed_next_pass() {
    let scratch = inactive_one("short_expires_in");
    let endpoint = FakeEndpoint::answering(vec![
        granted("fake-access-short", Some("fake-refresh-short"), 60),
        granted("fake-access-new", Some("fake-refresh-new"), 3600),
    ]);
    let rig = Rig::new(scratch, endpoint);

    let first = rig.consume(&expired());

    assert_eq!(first.error, None);
    let oauth = stored_oauth(&rig);
    assert_eq!(oauth["refreshToken"], "fake-refresh-short");
    assert_eq!(
        oauth["expiresAt"],
        ms_after(60),
        "the short lifetime is written as is"
    );

    let short = rig.stored().expect("the slot holds a credential");
    let second = rig.consume(&short);

    assert_eq!(second.error, None);
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-one", "fake-refresh-short"],
        "the short-lived token is refreshed on the next pass"
    );
    assert_eq!(stored_oauth(&rig)["accessToken"], "fake-access-new");
}
