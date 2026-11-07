//! `cargo run -p protocol-ts`: regenerates `apps/pwa/src/protocol.ts` from
//! `crates/protocol`'s source. Exits non-zero with the error on failure.

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let root = protocol_ts::repo_root()?;
    let generated = protocol_ts::generate(&root)?;
    let out = root.join(protocol_ts::OUTPUT_PATH);
    std::fs::write(&out, generated).with_context(|| format!("writing {}", out.display()))
}
