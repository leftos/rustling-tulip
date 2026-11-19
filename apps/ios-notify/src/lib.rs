//! The phone app's notification service extension, as plain Rust: what a
//! relay push shows once its ciphertext decrypts. The iOS binary
//! (`src/main.rs`) wraps it in the extension class. [`clearing`] is the rule
//! for which delivered Needs you notifications a notice ends, shared by the
//! extension and the app's background clear.

pub mod clearing;
pub mod notice;
