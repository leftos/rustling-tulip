//! On iOS the extension's executable starts in Foundation's `NSExtensionMain`,
//! not in a Rust `main`, and links the frameworks its class uses. Builds for
//! other targets add nothing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "ios") {
        println!("cargo:rustc-link-arg-bins=-e");
        println!("cargo:rustc-link-arg-bins=_NSExtensionMain");
        println!("cargo:rustc-link-arg-bins=-framework");
        println!("cargo:rustc-link-arg-bins=Foundation");
        println!("cargo:rustc-link-arg-bins=-framework");
        println!("cargo:rustc-link-arg-bins=UserNotifications");
    }
}
