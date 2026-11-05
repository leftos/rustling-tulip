//! The claude-accounts crate's usage table, OAuth credential reading and usage polling against a fake account store.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot set up its files or take its lock must fail loudly"
)]

#[cfg(accounts_locks)]
mod entries;
mod oauth;
#[cfg(accounts_locks)]
mod poll;
#[cfg(accounts_locks)]
mod support;
