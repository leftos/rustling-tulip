//! The phone's Files: picks one document for the composer to upload.
//!
//! On iOS the picker is `UIDocumentPickerViewController` (`files_ios.rs`);
//! every other platform logs the request and picks nothing, and
//! [`FakeFilePicker`] answers as a spec says.

use std::sync::{Mutex, PoisonError};

use futures::FutureExt as _;
use futures::future::BoxFuture;

/// The Attach sheet's Files row's label.
pub const FILES_LABEL: &str = "Files";

/// A picked file's name and bytes.
pub type PickedFile = (String, Vec<u8>);

/// What the phone's platform offers for picking a file.
pub trait FilePicker: Send + Sync {
    /// Shows the platform's document picker for one file; answers the picked
    /// file's name and bytes, or `None` when the pick was cancelled or failed.
    fn pick(&self) -> BoxFuture<'static, Option<PickedFile>>;
}

/// A platform with no file picker: a request is logged and nothing is
/// picked.
pub struct LoggingFiles;

impl FilePicker for LoggingFiles {
    fn pick(&self) -> BoxFuture<'static, Option<PickedFile>> {
        tracing::info!("phone: no file picker on this platform");
        futures::future::ready(None).boxed()
    }
}

/// A file picker for specs: every pick answers what it was made with, and
/// the picks are counted.
pub struct FakeFilePicker {
    answer: Option<PickedFile>,
    picks: Mutex<usize>,
}

impl FakeFilePicker {
    /// A picker whose every pick answers `answer`.
    #[must_use]
    pub fn answering(answer: Option<PickedFile>) -> Self {
        Self {
            answer,
            picks: Mutex::new(0),
        }
    }

    /// How many picks were asked for.
    #[must_use]
    pub fn picks(&self) -> usize {
        *self.picks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl FilePicker for FakeFilePicker {
    fn pick(&self) -> BoxFuture<'static, Option<PickedFile>> {
        *self.picks.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        futures::future::ready(self.answer.clone()).boxed()
    }
}
