//! The claude-accounts crate's token refresh: the token POST and the credential fingerprints against a fake token endpoint,
//! and the consume gate against a fake endpoint, a fake session table and a scratch account store.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot set up its files or take its lock must fail loudly"
)]

#[cfg(accounts_locks)]
mod active;
#[cfg(accounts_locks)]
mod capture;
#[cfg(accounts_locks)]
mod freshen;
#[cfg(accounts_locks)]
mod gate;
mod grant;
#[cfg(accounts_locks)]
#[path = "../support/links.rs"]
mod links;
#[cfg(accounts_locks)]
mod poll;
#[cfg(accounts_locks)]
mod stash;
mod support;
#[cfg(accounts_locks)]
mod switch;
#[cfg(all(windows, target_pointer_width = "64"))]
#[path = "../support/windows_files.rs"]
mod windows_files;
