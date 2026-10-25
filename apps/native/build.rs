//! Embeds the app icon in the exe as icon resource 1: GPUI loads that resource for the window and taskbar icon on
//! Windows, and Explorer and shortcuts show it too. Builds for other targets embed nothing.

fn main() -> Result<(), embed_resource::CompilationResult> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=../../tools/installer/icon.ico");
    embed_resource::compile("app.rc", embed_resource::NONE).manifest_required()
}
