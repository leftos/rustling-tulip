//! The checked-in `apps/pwa/src/protocol.ts` is what the generator emits
//! from the protocol crate as it stands.

#![expect(clippy::expect_used, reason = "tests assert preconditions with expect")]

#[test]
fn generated_protocol_ts_is_current() {
    let root = protocol_ts::repo_root().expect("the workspace root resolves");
    let generated = protocol_ts::generate(&root).expect("the protocol crate generates");
    let checked_in = std::fs::read(root.join(protocol_ts::OUTPUT_PATH)).unwrap_or_default();
    assert!(
        checked_in == generated.as_bytes(),
        "apps/pwa/src/protocol.ts is stale: run `cargo run -p protocol-ts` through the gate and commit the result"
    );
}
