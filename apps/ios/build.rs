//! A simulator build that `tools/ios/build.ps1` makes links the app's
//! simulator entitlements into the binary as the `__TEXT,__entitlements` and
//! `__TEXT,__ents_der` sections, which an ad hoc signed simulator bundle needs
//! to reach its Keychain. Every other build adds nothing.

use std::path::Path;

/// The folder holding `app.xml` and `app.der`, set by `tools/ios/build.ps1`
/// for a simulator build only.
const ENTITLEMENTS_VAR: &str = "RUSTLING_TULIP_IOS_SIM_ENTITLEMENTS";

fn main() -> Result<(), String> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={ENTITLEMENTS_VAR}");
    let Some(dir) = std::env::var_os(ENTITLEMENTS_VAR).filter(|dir| !dir.is_empty()) else {
        return Ok(());
    };
    if std::env::var("TARGET").ok().as_deref() != Some("aarch64-apple-ios-sim") {
        return Ok(());
    }
    for (section, extension) in [("__entitlements", "xml"), ("__ents_der", "der")] {
        let file = Path::new(&dir).join(format!("app.{extension}"));
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
