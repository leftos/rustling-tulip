//! The claude-accounts crate's account store, file locks and secret types against a hand-written fake account store.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot set up its files or take its lock must fail loudly"
)]

#[cfg(all(windows, target_pointer_width = "64"))]
mod accounts_json;
#[cfg(all(windows, target_pointer_width = "64"))]
mod add;
#[cfg(all(windows, target_pointer_width = "64"))]
mod claude_files;
#[cfg(all(windows, target_pointer_width = "64"))]
mod credentials;
#[cfg(all(windows, target_pointer_width = "64"))]
mod disable;
mod guard;
#[cfg(all(windows, target_pointer_width = "64"))]
#[path = "../support/links.rs"]
mod links;
#[cfg(all(windows, target_pointer_width = "64"))]
mod locks;
#[cfg(all(windows, target_pointer_width = "64"))]
mod replace;
#[cfg(all(windows, target_pointer_width = "64"))]
mod root_locks;
mod seal;
#[cfg(all(windows, target_pointer_width = "64"))]
mod secrets;
mod support;
#[cfg(not(all(windows, target_pointer_width = "64")))]
mod unsupported;
#[cfg(all(windows, target_pointer_width = "64"))]
mod validation;
#[cfg(all(windows, target_pointer_width = "64"))]
#[path = "../support/windows_files.rs"]
mod windows_files;
