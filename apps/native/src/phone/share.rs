//! The phone's Share: hands a fetched copy to the platform's share sheet.
//!
//! On iOS the sheet is `UIActivityViewController` (`share_ios.rs`); every
//! other platform logs the request, and [`FakeShareSheet`] records it so the
//! UI specs prove what is shared.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// The viewer header's Share button label.
pub const SHARE_LABEL: &str = "Share";

/// What the phone's platform offers for sharing a file.
pub trait ShareSheet: Send + Sync {
    /// Presents the platform's share sheet for the file at `path`.
    fn present(&self, path: &Path);
}

/// A platform with no share sheet: a request is logged and nothing shows.
pub struct LoggingShare;

impl ShareSheet for LoggingShare {
    fn present(&self, path: &Path) {
        tracing::info!(path = %path.display(), "phone: no share sheet on this platform");
    }
}

/// A share sheet for specs: records every path it was asked to share.
#[derive(Default)]
pub struct FakeShareSheet {
    shared: Mutex<Vec<PathBuf>>,
}

impl FakeShareSheet {
    /// Every path shared so far, in order.
    #[must_use]
    pub fn shared(&self) -> Vec<PathBuf> {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ShareSheet for FakeShareSheet {
    fn present(&self, path: &Path) {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(path.to_path_buf());
    }
}
