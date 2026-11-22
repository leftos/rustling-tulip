//! Launched with [`KEYCHAIN_SELFTEST_ARG`], the app saves a token for a
//! made-up host in the device token store pairing uses, reads it back and
//! removes it, off the main thread, and logs whether the Keychain held it:
//! the simulator's run check (`tools/ios/sim-run.ps1`) passes the argument.

use anyhow::{Context as _, Result, bail};
use daemon_client::device_tokens::{DeviceTokenStore, KeyringTokens};

/// The launch argument that runs the Keychain round trip once at start.
pub const KEYCHAIN_SELFTEST_ARG: &str = "--keychain-selftest";

/// The host the self-test files its token under; `.invalid` names no real one.
const SELFTEST_HOST: &str = "keychain-selftest.invalid";

/// The token the self-test saves and expects back.
const SELFTEST_TOKEN: &str = "selftest";

/// Runs the round trip on a thread of its own and logs
/// `phone: keychain self-test passed` or `phone: keychain self-test failed`.
pub fn start() {
    let spawned = std::thread::Builder::new()
        .name("keychain-selftest".to_owned())
        .spawn(|| match round_trip() {
            Ok(()) => tracing::info!("phone: keychain self-test passed"),
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "phone: keychain self-test failed");
            }
        });
    if let Err(err) = spawned {
        tracing::warn!(error = %err, "phone: keychain self-test failed");
    }
}

/// Save, read back and remove [`SELFTEST_TOKEN`]; the first step that fails,
/// or a read that returns anything else, is the error.
fn round_trip() -> Result<()> {
    let store = KeyringTokens::open().context("opening the device token store")?;
    store.set(SELFTEST_HOST, SELFTEST_TOKEN)?;
    let read = store.get(SELFTEST_HOST)?;
    if read.as_deref() != Some(SELFTEST_TOKEN) {
        bail!("the token read back was {read:?}, not {SELFTEST_TOKEN:?}");
    }
    store.delete(SELFTEST_HOST)
}
