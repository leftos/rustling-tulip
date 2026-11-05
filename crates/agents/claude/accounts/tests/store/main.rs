//! The claude-accounts crate's account store, file locks and secret types against a hand-written fake account store.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot set up its files or take its lock must fail loudly"
)]

#[cfg(accounts_locks)]
mod accounts_json;
#[cfg(accounts_locks)]
mod add;
#[cfg(accounts_locks)]
mod alias;
#[cfg(accounts_locks)]
mod claude_files;
#[cfg(accounts_locks)]
mod credentials;
#[cfg(accounts_locks)]
mod disable;
mod guard;
#[cfg(accounts_locks)]
#[path = "../support/links.rs"]
mod links;
#[cfg(accounts_locks)]
mod locks;
#[cfg(accounts_locks)]
mod remove;
#[cfg(accounts_locks)]
mod replace;
#[cfg(accounts_locks)]
mod root_locks;
mod seal;
#[cfg(accounts_locks)]
mod secrets;
#[cfg(accounts_locks)]
mod settings;
mod support;
#[cfg(not(accounts_locks))]
mod unsupported;
#[cfg(accounts_locks)]
mod validation;
#[cfg(accounts_locks)]
#[path = "../support/windows_files.rs"]
mod windows_files;
