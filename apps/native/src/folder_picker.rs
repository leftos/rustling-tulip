//! The client's folder picker: the OS folder dialog, opened at a start
//! folder when one is given.
//!
//! The Windows dialog is adapted from GPUI 0.2.2's `file_open_dialog`
//! (`gpui/src/platform/windows/platform.rs`, Apache-2.0, Copyright Zed
//! Industries, Inc.). GPUI's `PathPromptOptions` has no start folder, so this
//! copy picks one folder and points the dialog at the start folder with
//! `SHCreateItemFromParsingName` and `IFileDialog::SetFolder`, as GPUI's own
//! `file_save_dialog` does.

use std::path::PathBuf;
use std::rc::Rc;

use futures::FutureExt as _;
use gpui::{App, Window};

use crate::{FilePicker, FolderPicker};

/// What the OS dialog asks for.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Folder,
    File,
}

/// How long the start folder's existence check may take before the dialog
/// opens without it: a folder on an offline network share would otherwise
/// hold the dialog back for the share's timeout.
#[cfg(windows)]
const START_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The OS folder picker, modal to `window`. The start folder is checked on
/// the background executor; the dialog then runs on the foreground
/// executor, as GPUI runs its own, so its message loop never runs inside a
/// view's update.
#[cfg(windows)]
pub(crate) fn system_folder_picker(window: &Window) -> FolderPicker {
    system_picker(window, PickerKind::Folder)
}

/// The OS file picker, filtered to `.code-workspace` files and opened at the
/// given folder: the workspace creator's Browse. Same threading rules as
/// [`system_folder_picker`].
#[cfg(windows)]
pub(crate) fn system_file_picker(window: &Window) -> FilePicker {
    system_picker(window, PickerKind::File)
}

/// The shared body of the two pickers: check the start folder, then show the
/// dialog on the foreground executor.
#[cfg(windows)]
fn system_picker(window: &Window, kind: PickerKind) -> FolderPicker {
    let owner = owner_address(window);
    Rc::new(move |cx: &mut App, start: Option<PathBuf>| {
        let checked = start.map(|start| {
            cx.background_executor().spawn(async move {
                start_folder_check(start, START_CHECK_TIMEOUT, std::path::Path::is_dir)
            })
        });
        let foreground = cx.foreground_executor().clone();
        async move {
            let start = match checked {
                Some(check) => check.await,
                None => None,
            };
            foreground
                .spawn(async move {
                    match pick_dialog(kind, start.as_deref(), owner) {
                        Ok(picked) => picked,
                        Err(err) => {
                            tracing::warn!("the picker failed: {err:#}");
                            None
                        }
                    }
                })
                .await
        }
        .boxed_local()
    })
}

/// `start` when `probe` says it is a folder within `timeout`, else `None`
/// with a warning. The probe runs on a thread of its own, so a check stuck
/// on an unreachable share is abandoned rather than waited out.
#[cfg(windows)]
fn start_folder_check(
    start: PathBuf,
    timeout: std::time::Duration,
    probe: impl FnOnce(&std::path::Path) -> bool + Send + 'static,
) -> Option<PathBuf> {
    let (tx, rx) = std::sync::mpsc::channel();
    let probed = start.clone();
    let spawned = std::thread::Builder::new()
        .name("folder-picker-start".to_owned())
        .spawn(move || {
            if tx.send(probe(&probed)).is_err() {
                tracing::debug!(
                    "the start folder check of {} ended after its wait",
                    probed.display()
                );
            }
        });
    let reason = match spawned.map(|_| rx.recv_timeout(timeout)) {
        Ok(Ok(true)) => return Some(start),
        Ok(Ok(false)) => "it is not a folder".to_owned(),
        Ok(Err(std::sync::mpsc::RecvTimeoutError::Timeout)) => {
            format!("checking it took over {} ms", timeout.as_millis())
        }
        Ok(Err(std::sync::mpsc::RecvTimeoutError::Disconnected)) => {
            "its check stopped without an answer".to_owned()
        }
        Err(err) => format!("its check could not start: {err}"),
    };
    tracing::warn!(
        "the folder picker opens at its own folder, not {}: {reason}",
        start.display()
    );
    None
}

/// The platform's folder picker through GPUI, which takes no start folder.
#[cfg(not(windows))]
pub(crate) fn system_folder_picker(_window: &Window) -> FolderPicker {
    system_picker(false)
}

/// The platform's file picker through GPUI, with the same filter caveat as
/// GPUI's own: the platform shows any file.
#[cfg(not(windows))]
pub(crate) fn system_file_picker(_window: &Window) -> FilePicker {
    system_picker(true)
}

#[cfg(not(windows))]
fn system_picker(files: bool) -> FolderPicker {
    Rc::new(move |cx: &mut App, _start: Option<PathBuf>| {
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files,
            directories: !files,
            multiple: false,
            prompt: None,
        });
        async move {
            match picked.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Err(err)) => {
                    tracing::warn!("the picker failed: {err:#}");
                    None
                }
                // Cancelled, or the picker's sender went before it answered.
                Ok(Ok(None)) | Err(_) => None,
            }
        }
        .boxed_local()
    })
}

/// The address of `window`'s Win32 handle, which owns the dialog; `None`
/// opens the dialog unowned.
#[cfg(windows)]
fn owner_address(window: &Window) -> Option<usize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
    let raw = HasWindowHandle::window_handle(window).map(|handle| handle.as_raw());
    match raw {
        Ok(RawWindowHandle::Win32(handle)) => Some(handle.hwnd.get().cast_unsigned()),
        other => {
            tracing::warn!(
                "the folder picker opens unowned: the window has no Win32 handle ({other:?})"
            );
            None
        }
    }
}

/// Shows the folder or `.code-workspace` file dialog, at `start` (a folder
/// that passed [`start_folder_check`]) when given, modal to the window at
/// `owner`; `None` when the user cancels.
#[cfg(windows)]
fn pick_dialog(
    kind: PickerKind,
    start: Option<&std::path::Path>,
    owner: Option<usize>,
) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context as _;
    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
    use windows::Win32::UI::Shell::{
        Common::COMDLG_FILTERSPEC, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
        FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
        SIGDN_FILESYSPATH,
    };
    use windows::core::{HSTRING, w};

    // SAFETY: COM is initialised on this thread: GPUI's Windows platform
    // calls `OleInitialize(None)` on the UI thread when it starts, and the
    // picker runs on that thread's foreground executor.
    let dialog: IFileOpenDialog = unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL) }
        .context("creating the picker dialog")?;
    let options = match kind {
        PickerKind::Folder => FOS_FILEMUSTEXIST | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM,
        PickerKind::File => FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM,
    };
    // SAFETY: `dialog` is a live dialog this thread created.
    unsafe { dialog.SetOptions(options) }.context("setting the picker dialog's options")?;
    if kind == PickerKind::File {
        let filters = [COMDLG_FILTERSPEC {
            pszName: w!("VS Code workspace"),
            pszSpec: w!("*.code-workspace"),
        }];
        // SAFETY: `dialog` is live and `filters` outlives the call.
        unsafe { dialog.SetFileTypes(&filters) }.context("setting the file dialog's filter")?;
    }
    if let Some(start) = start {
        let path = HSTRING::from(start.as_os_str());
        // SAFETY: `path` is a NUL-terminated wide string that outlives the
        // call, and no bind context is passed.
        let item: windows::core::Result<IShellItem> =
            unsafe { SHCreateItemFromParsingName(&path, None) };
        // SAFETY: `dialog` is live and `item` is a shell item the call
        // above created.
        let set = item.and_then(|item| unsafe { dialog.SetFolder(&item) });
        if let Err(err) = set {
            tracing::warn!(
                "the picker opens at its own folder, not {}: {err}",
                start.display()
            );
        }
    }
    let owner = owner.map(|address| HWND(std::ptr::with_exposed_provenance_mut(address)));
    // SAFETY: `dialog` is live, and `owner`, when given, is the client's
    // main window, which lives as long as the app.
    if let Err(err) = unsafe { dialog.Show(owner) } {
        if err.code() == ERROR_CANCELLED.to_hresult() {
            return Ok(None);
        }
        return Err(err).context("showing the picker dialog");
    }
    // SAFETY: `dialog` is live and was shown and confirmed.
    let results = unsafe { dialog.GetResults() }.context("reading the picked item")?;
    // SAFETY: `results` is the live item array the dialog returned.
    if unsafe { results.GetCount() }.context("counting the picked items")? == 0 {
        return Ok(None);
    }
    // SAFETY: `results` holds at least one item.
    let item = unsafe { results.GetItemAt(0) }.context("reading the picked item")?;
    // SAFETY: `item` is a live shell item; the dialog forced file-system
    // items, so it has a file-system path.
    let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }
        .context("reading the picked item's path")?;
    // SAFETY: `name` is the NUL-terminated string the shell allocated for
    // the call above.
    let path = unsafe { name.to_string() };
    // SAFETY: `name` was allocated with the COM task allocator and is not
    // used after this.
    unsafe { CoTaskMemFree(Some(name.0.cast_const().cast())) };
    let path = path.context("the picked item's path is not valid UTF-16")?;
    Ok(Some(PathBuf::from(path)))
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::start_folder_check;

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn an_existing_folder_is_kept() {
        let dir = std::env::temp_dir();
        assert_eq!(
            start_folder_check(dir.clone(), WAIT, Path::is_dir),
            Some(dir)
        );
    }

    #[test]
    fn a_missing_folder_is_dropped() {
        let missing = std::env::temp_dir().join("rt-folder-picker-no-such-folder");
        assert_eq!(start_folder_check(missing, WAIT, Path::is_dir), None);
    }

    #[test]
    fn a_check_that_outlasts_its_wait_is_dropped() {
        let stuck = |_: &Path| {
            std::thread::sleep(Duration::from_millis(500));
            true
        };
        let start = PathBuf::from("\\\\offline-host\\share");
        assert_eq!(
            start_folder_check(start, Duration::from_millis(20), stuck),
            None,
            "an unreachable share is not waited out"
        );
    }
}
