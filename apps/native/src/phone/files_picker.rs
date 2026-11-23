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

/// What a file pick answers: the file read, or the name of one over the
/// upload cap, which is never read into memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePick {
    /// The picked file's name and bytes.
    File(PickedFile),
    /// The name of a picked file over the upload cap.
    TooLarge(String),
}

/// What the phone's platform offers for picking a file.
pub trait FilePicker: Send + Sync {
    /// Shows the platform's document picker for one file; answers the picked
    /// file's name and bytes, or its name alone when it is over the upload
    /// cap, or `None` when the pick was cancelled or failed.
    fn pick(&self) -> BoxFuture<'static, Option<FilePick>>;
}

/// A platform with no file picker: a request is logged and nothing is
/// picked.
pub struct LoggingFiles;

impl FilePicker for LoggingFiles {
    fn pick(&self) -> BoxFuture<'static, Option<FilePick>> {
        tracing::info!("phone: no file picker on this platform");
        futures::future::ready(None).boxed()
    }
}

/// A file picker for specs: every pick answers what it was made with, and
/// the picks are counted.
pub struct FakeFilePicker {
    answer: Option<FilePick>,
    picks: Mutex<usize>,
}

impl FakeFilePicker {
    /// A picker whose every pick answers `answer`.
    #[must_use]
    pub fn answering(answer: Option<FilePick>) -> Self {
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
    fn pick(&self) -> BoxFuture<'static, Option<FilePick>> {
        *self.picks.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        futures::future::ready(self.answer.clone()).boxed()
    }
}
