//! Launched with [`SHARE_INBOX_SELFTEST_ARG`], the app finds the share inbox
//! in the App Group container, writes one text item there with the share
//! extension's own writer, reads it back with the app's inbox reader and
//! removes it, off the main thread, and logs whether the item came back as
//! written: the simulator's run check (`tools/ios/sim-run.ps1`) passes the
//! argument.

use std::time::SystemTime;

use anyhow::{Context as _, Result, bail};
use rustling_tulip_ios_share::{Inbox, TextKind, now_ms};

use super::inbox::{self, Kind};

/// The launch argument that runs the share inbox round trip once at start.
pub const SHARE_INBOX_SELFTEST_ARG: &str = "--share-inbox-selftest";

/// The text the self-test writes and expects back.
const SELFTEST_TEXT: &str = "share inbox self-test";

/// Runs the round trip on a thread of its own and logs
/// `phone: share inbox self-test passed` or
/// `phone: share inbox self-test failed`.
pub fn start() {
    let spawned = std::thread::Builder::new()
        .name("share-inbox-selftest".to_owned())
        .spawn(|| match round_trip() {
            Ok(()) => tracing::info!("phone: share inbox self-test passed"),
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "phone: share inbox self-test failed");
            }
        });
    if let Err(err) = spawned {
        tracing::warn!(error = %err, "phone: share inbox self-test failed");
    }
}

/// Write [`SELFTEST_TEXT`] into the App Group's inbox, find it among the
/// ready items the app reads there, check its kind, text and id, and remove
/// its files; the first step that fails, or an item that differs, is the
/// error.
fn round_trip() -> Result<()> {
    let dir = super::inbox_ios::group_container()
        .context("the system names no App Group container for the app")?;
    let id = Inbox::new(dir.clone())
        .write_text(TextKind::Text, SELFTEST_TEXT, now_ms())
        .with_context(|| format!("writing a text item into {}", dir.display()))?;
    let scan = inbox::scan(&dir, SystemTime::now());
    let Some(item) = scan.ready.iter().find(|item| item.manifest.id == id) else {
        bail!(
            "item {id} was not among the ready items in {}",
            dir.display()
        );
    };
    let manifest = &item.manifest;
    if manifest.kind != Kind::Text || manifest.text.as_deref() != Some(SELFTEST_TEXT) {
        bail!(
            "item {id} read back as {:?} holding {:?}, not text holding {SELFTEST_TEXT:?}",
            manifest.kind,
            manifest.text
        );
    }
    inbox::remove(item).with_context(|| format!("removing item {id}"))
}
