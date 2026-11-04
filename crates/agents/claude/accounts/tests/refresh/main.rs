//! The claude-accounts crate's token refresh: the token POST and cswap's fingerprints against a fake token endpoint,
//! and the consume gate against a fake endpoint, a fake session table and a scratch account store.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot set up its files or take its lock must fail loudly"
)]

#[cfg(all(windows, target_pointer_width = "64"))]
mod active;
#[cfg(all(windows, target_pointer_width = "64"))]
mod capture;
#[cfg(all(windows, target_pointer_width = "64"))]
mod freshen;
#[cfg(all(windows, target_pointer_width = "64"))]
mod gate;
mod grant;
#[cfg(all(windows, target_pointer_width = "64"))]
#[path = "../support/links.rs"]
mod links;
#[cfg(all(windows, target_pointer_width = "64"))]
mod poll;
#[cfg(all(windows, target_pointer_width = "64"))]
mod stash;
mod support;
#[cfg(all(windows, target_pointer_width = "64"))]
mod switch;
#[cfg(all(windows, target_pointer_width = "64"))]
#[path = "../support/windows_files.rs"]
mod windows_files;
