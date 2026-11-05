//! Off 64-bit Windows and macOS the account locks are unsupported, so no store write can be taken under one, and
//! taking one makes nothing for a lock it cannot have.

use claude_accounts::{AccountLock, ConsumeLock, LockError};

use crate::support::Scratch;

#[test]
fn account_lock_is_unsupported_without_locks() {
    let scratch = Scratch::empty("account_lock_is_unsupported_without_locks");

    let error = AccountLock::acquire(&scratch.root())
        .expect_err("the account lock is unsupported on this platform");

    assert!(matches!(error, LockError::Unsupported));
}

#[test]
fn the_account_lock_creates_nothing_without_locks() {
    let scratch = Scratch::empty("the_account_lock_creates_nothing_without_locks");

    let error = AccountLock::acquire(&scratch.root())
        .expect_err("the account lock is unsupported on this platform");

    assert!(matches!(error, LockError::Unsupported));
    assert!(
        !scratch.backup().exists(),
        "no account store root was made for the lock"
    );
}

#[test]
fn the_consume_lock_creates_nothing_without_locks() {
    let scratch = Scratch::empty("the_consume_lock_creates_nothing_without_locks");
    let root = scratch.root();

    let error = ConsumeLock::acquire(&root, 1)
        .expect_err("the consume lock is unsupported on this platform");

    assert!(matches!(error, LockError::Unsupported));
    assert!(
        !root.credentials_dir().exists(),
        "no credentials folder was made for the lock"
    );
    assert!(
        !scratch.backup().exists(),
        "no account store root was made for the lock"
    );
}
