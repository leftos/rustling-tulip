//! Sets the `accounts_locks` cfg on the platforms where the store's and Claude Code's locks are implemented: 64-bit
//! Windows and macOS. Everywhere else every lock returns `LockError::Unsupported`, and the code and tests that need a
//! lock are left out.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(accounts_locks)");
    println!("cargo::rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let width = std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_default();
    if os == "macos" || (os == "windows" && width == "64") {
        println!("cargo::rustc-cfg=accounts_locks");
    }
}
