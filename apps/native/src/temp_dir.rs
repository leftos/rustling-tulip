//! A scratch folder for tests, removed when the test ends.

#![expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A fresh folder under the system temp dir, removed on drop.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    /// Makes a new folder named after `label`, so one test's folder can be
    /// told from another's.
    pub(crate) fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rt-native-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&path).expect("create the temp folder");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
