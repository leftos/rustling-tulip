//! On iOS the extension's executable starts in Foundation's `NSExtensionMain`,
//! not in a Rust `main`, and links the frameworks its class uses. A simulator
//! build that `tools/ios/build.ps1` makes also links the extension's simulator
//! entitlements in as the `__TEXT,__entitlements` and `__TEXT,__ents_der`
//! sections, which an ad hoc signed simulator bundle needs to reach its
//! App Group. Builds for other targets add nothing.

use std::path::Path;

/// The folder holding `share.xml` and `share.der`, set by
/// `tools/ios/build.ps1` for a simulator build only.
const ENTITLEMENTS_VAR: &str = "RUSTLING_TULIP_IOS_SIM_ENTITLEMENTS";

fn main() -> Result<(), String> {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "ios") {
        println!("cargo:rustc-link-arg-bins=-e");
        println!("cargo:rustc-link-arg-bins=_NSExtensionMain");
        println!("cargo:rustc-link-arg-bins=-framework");
        println!("cargo:rustc-link-arg-bins=Foundation");
        println!("cargo:rustc-link-arg-bins=-framework");
        println!("cargo:rustc-link-arg-bins=UIKit");
    }
    link_sim_entitlements()
}

/// The section link arguments for a simulator build with [`ENTITLEMENTS_VAR`]
/// set; a file it names that is missing is the error.
fn link_sim_entitlements() -> Result<(), String> {
    println!("cargo:rerun-if-env-changed={ENTITLEMENTS_VAR}");
    let Some(dir) = std::env::var_os(ENTITLEMENTS_VAR).filter(|dir| !dir.is_empty()) else {
        return Ok(());
    };
    if std::env::var("TARGET").ok().as_deref() != Some("aarch64-apple-ios-sim") {
        return Ok(());
    }
    for (section, extension) in [("__entitlements", "xml"), ("__ents_der", "der")] {
        let file = Path::new(&dir).join(format!("share.{extension}"));
        if !file.is_file() {
            return Err(format!(
                "{ENTITLEMENTS_VAR} is set but {} does not exist",
                file.display()
            ));
        }
        println!("cargo:rerun-if-changed={}", file.display());
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,{section},{}",
            file.display()
        );
    }
    Ok(())
}
