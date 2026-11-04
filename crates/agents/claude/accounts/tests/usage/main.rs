//! The claude-accounts crate's usage table, OAuth credential reading and usage polling against a fake account store.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot set up its files or take its lock must fail loudly"
)]

#[cfg(all(windows, target_pointer_width = "64"))]
mod entries;
mod oauth;
#[cfg(all(windows, target_pointer_width = "64"))]
mod poll;
#[cfg(all(windows, target_pointer_width = "64"))]
mod support;
