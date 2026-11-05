//! The claude-accounts crate's auto-switch engine files: the state file and the engine lease, against a scratch
//! account store.

#![cfg_attr(
    accounts_locks,
    expect(
        clippy::expect_used,
        reason = "a test that cannot set up its files or take its lock must fail loudly"
    )
)]

#[cfg(accounts_locks)]
mod balance;
mod cadence;
#[cfg(accounts_locks)]
mod cadence_tick;
#[cfg(accounts_locks)]
mod collect;
#[cfg(accounts_locks)]
mod lease;
#[cfg(accounts_locks)]
mod paid;
#[cfg(accounts_locks)]
mod rank;
#[cfg(accounts_locks)]
mod reading;
#[cfg(accounts_locks)]
mod state;
#[cfg(accounts_locks)]
mod support;
#[cfg(accounts_locks)]
mod tick;
#[cfg(accounts_locks)]
mod trigger;
