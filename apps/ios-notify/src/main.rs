//! The phone app's notification service extension. On iOS it is an app
//! extension whose entry point is Foundation's `NSExtensionMain` (set by
//! `build.rs`), which loads the principal class `RTNotificationService`;
//! elsewhere it is an empty program, so the workspace builds on every host.

#![cfg_attr(target_os = "ios", no_main)]

#[cfg(target_os = "ios")]
mod appex;

#[cfg(not(target_os = "ios"))]
fn main() {}
