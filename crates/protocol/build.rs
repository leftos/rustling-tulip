use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")?;
    let workspace_root = PathBuf::from(&manifest_dir)
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or("could not resolve workspace root from CARGO_MANIFEST_DIR")?
        .to_path_buf();
    let json_path = workspace_root.join("protocol-version.json");

    println!("cargo:rerun-if-changed={}", json_path.display());
    println!("cargo:rerun-if-changed=build.rs");

    let contents = fs::read_to_string(&json_path)
        .map_err(|e| format!("failed to read {}: {e}", json_path.display()))?;
    let parsed: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|e| format!("invalid JSON in {}: {e}", json_path.display()))?;
    let version_u64 = parsed
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            format!(
                "missing or non-integer `version` field in {}",
                json_path.display()
            )
        })?;
    let version: u32 = version_u64
        .try_into()
        .map_err(|_| format!("`version` {version_u64} doesn't fit in u32"))?;

    let supported_array = parsed
        .get("supported")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            format!(
                "missing or non-array `supported` field in {}",
                json_path.display()
            )
        })?;
    let mut supported: Vec<u32> = Vec::with_capacity(supported_array.len());
    for entry in supported_array {
        let n = entry
            .as_u64()
            .ok_or_else(|| format!("non-integer entry in `supported` in {}", json_path.display()))?;
        let v: u32 = n
            .try_into()
            .map_err(|_| format!("`supported` entry {n} doesn't fit in u32"))?;
        supported.push(v);
    }
    if !supported.contains(&version) {
        return Err(format!(
            "`supported` must include the current `version` ({version}) in {}",
            json_path.display()
        )
        .into());
    }

    let supported_literal = supported
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");

    let out_dir = env::var("OUT_DIR")?;
    let dest = PathBuf::from(out_dir).join("protocol_version.rs");
    fs::write(
        &dest,
        format!(
            "pub const PROTOCOL_VERSION: u32 = {version};\n\
             pub const SUPPORTED_PROTOCOL_VERSIONS: &[u32] = &[{supported_literal}];\n"
        ),
    )
    .map_err(|e| format!("failed to write {}: {e}", dest.display()))?;
    Ok(())
}
