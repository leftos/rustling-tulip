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

use crate::{FileFilter, FilePicker, FolderPicker, SavePicker};

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
    let pick = system_picker(window);
    Rc::new(move |cx: &mut App, start: Option<PathBuf>| pick(cx, start, None))
}

/// The OS file picker, filtered to the given file types and opened at the
/// given folder: the workspace creator's Browse and the preset wizard's
/// file pickers. Same threading rules as [`system_folder_picker`].
#[cfg(windows)]
pub(crate) fn system_file_picker(window: &Window) -> FilePicker {
    let pick = system_picker(window);
    Rc::new(
        move |cx: &mut App, start: Option<PathBuf>, filter: FileFilter| {
            pick(cx, start, Some(filter))
        },
    )
}

/// The OS save dialog, modal to `window`, offering the filter's types with
/// the suggested file name filled in and asking before it replaces a file:
/// the Themes modal's Export. Same threading rules as
/// [`system_folder_picker`].
#[cfg(windows)]
pub(crate) fn system_save_picker(window: &Window) -> SavePicker {
    let owner = owner_address(window);
    Rc::new(
        move |cx: &mut App, start: Option<PathBuf>, suggested: String, filter: FileFilter| {
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
                        save_dialog(&filter, &suggested, start.as_deref(), owner)
                            .map(|picked| picked.map(|path| with_default_extension(path, &filter)))
                            .map_err(|err| {
                                tracing::warn!("the save picker failed: {err:#}");
                                format!("{err:#}")
                            })
                    })
                    .await
            }
            .boxed_local()
        },
    )
}

/// `path`, with the filter's first extension added when it has none.
fn with_default_extension(mut path: PathBuf, filter: &FileFilter) -> PathBuf {
    if path.extension().is_none()
        && let Some(ext) = filter.extensions.first()
    {
        path.set_extension(ext.strip_prefix('.').unwrap_or(ext));
    }
    path
}

/// A picker for a folder (no filter) or for a file of the filter's types.
#[cfg(windows)]
type FilteredPicker = Rc<
    dyn Fn(
        &mut App,
        Option<PathBuf>,
        Option<FileFilter>,
    ) -> futures::future::LocalBoxFuture<'static, Option<PathBuf>>,
>;

/// The shared body of the two pickers: check the start folder, then show the
/// dialog on the foreground executor.
#[cfg(windows)]
fn system_picker(window: &Window) -> FilteredPicker {
    let owner = owner_address(window);
    Rc::new(
        move |cx: &mut App, start: Option<PathBuf>, filter: Option<FileFilter>| {
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
                        match pick_dialog(filter.as_ref(), start.as_deref(), owner) {
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
        },
    )
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
    Rc::new(|cx: &mut App, _start: Option<PathBuf>| system_pick(cx, false))
}

/// The platform's file picker through GPUI, with the same filter caveat as
/// GPUI's own: the platform shows any file.
#[cfg(not(windows))]
pub(crate) fn system_file_picker(_window: &Window) -> FilePicker {
    Rc::new(|cx: &mut App, _start: Option<PathBuf>, _filter: FileFilter| system_pick(cx, true))
}

/// The platform's save dialog through GPUI, at the start folder (the
/// platform's own when there is none) with the suggested name; a chosen
/// name without an extension takes the filter's.
#[cfg(not(windows))]
pub(crate) fn system_save_picker(_window: &Window) -> SavePicker {
    Rc::new(
        |cx: &mut App, start: Option<PathBuf>, suggested: String, filter: FileFilter| {
            let picked = cx.prompt_for_new_path(&start.unwrap_or_default(), Some(&suggested));
            async move {
                match picked.await {
                    Ok(Ok(Some(path))) => Ok(Some(with_default_extension(path, &filter))),
                    Ok(Err(err)) => {
                        tracing::warn!("the save picker failed: {err:#}");
                        Err(format!("{err:#}"))
                    }
                    // Cancelled, or the picker's sender went before it answered.
                    Ok(Ok(None)) | Err(_) => Ok(None),
                }
            }
            .boxed_local()
        },
    )
}

#[cfg(not(windows))]
fn system_pick(
    cx: &mut App,
    files: bool,
) -> futures::future::LocalBoxFuture<'static, Option<PathBuf>> {
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

/// The dialog's filter: its name and its `;`-joined patterns, every file
/// when the filter names no extension. An extension written with its dot
/// (`.log`) is the same as one without.
#[cfg(windows)]
fn filter_spec(filter: &FileFilter) -> (String, String) {
    if filter.extensions.is_empty() {
        return (filter.name.clone(), "*.*".to_owned());
    }
    let patterns: Vec<String> = filter
        .extensions
        .iter()
        .map(|ext| format!("*.{}", ext.strip_prefix('.').unwrap_or(ext)))
        .collect();
    (filter.name.clone(), patterns.join(";"))
}

/// Shows the folder dialog (no filter) or the file dialog of the filter's
/// types, at `start` (a folder that passed [`start_folder_check`]) when
/// given, modal to the window at `owner`; `None` when the user cancels.
#[cfg(windows)]
fn pick_dialog(
    filter: Option<&FileFilter>,
    start: Option<&std::path::Path>,
    owner: Option<usize>,
) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context as _;
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        Common::COMDLG_FILTERSPEC, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
        FileOpenDialog, IFileOpenDialog,
    };
    use windows::core::{HSTRING, PCWSTR};

    // SAFETY: COM is initialised on this thread: GPUI's Windows platform
    // calls `OleInitialize(None)` on the UI thread when it starts, and the
    // picker runs on that thread's foreground executor.
    let dialog: IFileOpenDialog = unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL) }
        .context("creating the picker dialog")?;
    let options = if filter.is_some() {
        FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM
    } else {
        FOS_FILEMUSTEXIST | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM
    };
    // SAFETY: `dialog` is a live dialog this thread created.
    unsafe { dialog.SetOptions(options) }.context("setting the picker dialog's options")?;
    if let Some(filter) = filter {
        let (name, spec) = filter_spec(filter);
        let (name, spec) = (HSTRING::from(name), HSTRING::from(spec));
        let filters = [COMDLG_FILTERSPEC {
            pszName: PCWSTR(name.as_ptr()),
            pszSpec: PCWSTR(spec.as_ptr()),
        }];
        // SAFETY: `dialog` is live, and `filters` and the strings it points
        // into outlive the call.
        unsafe { dialog.SetFileTypes(&filters) }.context("setting the file dialog's filter")?;
    }
    if let Some(start) = start {
        set_start_folder(&dialog, start);
    }
    if !show_dialog(&dialog, owner)? {
        return Ok(None);
    }
    // SAFETY: `dialog` is live and was shown and confirmed.
    let results = unsafe { dialog.GetResults() }.context("reading the picked item")?;
    // SAFETY: `results` is the live item array the dialog returned.
    if unsafe { results.GetCount() }.context("counting the picked items")? == 0 {
        return Ok(None);
    }
    // SAFETY: `results` holds at least one item.
    let item = unsafe { results.GetItemAt(0) }.context("reading the picked item")?;
    item_path(&item).map(Some)
}

/// Shows the save dialog for a file of the filter's type, the suggested
/// name filled in and the filter's first extension added to a name typed
/// without one, at `start` when given, modal to the window at `owner`;
/// `None` when the user cancels. Replacing a file asks first.
#[cfg(windows)]
fn save_dialog(
    filter: &FileFilter,
    suggested: &str,
    start: Option<&std::path::Path>,
    owner: Option<usize>,
) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context as _;
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        Common::COMDLG_FILTERSPEC, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FileSaveDialog,
        IFileSaveDialog,
    };
    use windows::core::{HSTRING, PCWSTR};

    // SAFETY: COM is initialised on this thread, as for `pick_dialog`.
    let dialog: IFileSaveDialog = unsafe { CoCreateInstance(&FileSaveDialog, None, CLSCTX_ALL) }
        .context("creating the save dialog")?;
    // SAFETY: `dialog` is a live dialog this thread created.
    unsafe { dialog.SetOptions(FOS_OVERWRITEPROMPT | FOS_FORCEFILESYSTEM) }
        .context("setting the save dialog's options")?;
    let (name, spec) = filter_spec(filter);
    let (name, spec) = (HSTRING::from(name), HSTRING::from(spec));
    let filters = [COMDLG_FILTERSPEC {
        pszName: PCWSTR(name.as_ptr()),
        pszSpec: PCWSTR(spec.as_ptr()),
    }];
    // SAFETY: `dialog` is live, and `filters` and the strings it points into
    // outlive the call.
    unsafe { dialog.SetFileTypes(&filters) }.context("setting the save dialog's filter")?;
    if let Some(ext) = filter.extensions.first() {
        let ext = HSTRING::from(ext.strip_prefix('.').unwrap_or(ext));
        // SAFETY: `dialog` is live and `ext` outlives the call.
        unsafe { dialog.SetDefaultExtension(&ext) }
            .context("setting the save dialog's extension")?;
    }
    // SAFETY: `dialog` is live and the name outlives the call.
    unsafe { dialog.SetFileName(&HSTRING::from(suggested)) }
        .context("setting the save dialog's file name")?;
    if let Some(start) = start {
        set_start_folder(&dialog, start);
    }
    if !show_dialog(&dialog, owner)? {
        return Ok(None);
    }
    // SAFETY: `dialog` is live and was shown and confirmed.
    let item = unsafe { dialog.GetResult() }.context("reading the chosen file")?;
    item_path(&item).map(Some)
}

/// Points `dialog` at `start`, or leaves it at its own folder with a warning
/// when the shell cannot parse the path.
#[cfg(windows)]
fn set_start_folder(dialog: &windows::Win32::UI::Shell::IFileDialog, start: &std::path::Path) {
    use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName};
    use windows::core::HSTRING;

    let path = HSTRING::from(start.as_os_str());
    // SAFETY: `path` is a NUL-terminated wide string that outlives the
    // call, and no bind context is passed.
    let item: windows::core::Result<IShellItem> =
        unsafe { SHCreateItemFromParsingName(&path, None) };
    // SAFETY: `dialog` is live and `item` is a shell item the call above
    // created.
    let set = item.and_then(|item| unsafe { dialog.SetFolder(&item) });
    if let Err(err) = set {
        tracing::warn!(
            "the picker opens at its own folder, not {}: {err}",
            start.display()
        );
    }
}

/// Shows `dialog` modal to the window at `owner`: `true` once the user
/// confirms, `false` when they cancel.
#[cfg(windows)]
fn show_dialog(
    dialog: &windows::Win32::UI::Shell::IFileDialog,
    owner: Option<usize>,
) -> anyhow::Result<bool> {
    use anyhow::Context as _;
    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};

    let owner = owner.map(|address| HWND(std::ptr::with_exposed_provenance_mut(address)));
    // SAFETY: `dialog` is live, and `owner`, when given, is the client's
    // main window, which lives as long as the app.
    match unsafe { dialog.Show(owner) } {
        Ok(()) => Ok(true),
        Err(err) if err.code() == ERROR_CANCELLED.to_hresult() => Ok(false),
        Err(err) => Err(err).context("showing the picker dialog"),
    }
}

/// The file-system path of the item a dialog returned.
#[cfg(windows)]
fn item_path(item: &windows::Win32::UI::Shell::IShellItem) -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::SIGDN_FILESYSPATH;

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
    Ok(PathBuf::from(path))
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{filter_spec, start_folder_check, with_default_extension};
    use crate::FileFilter;

    #[test]
    fn a_saved_name_without_an_extension_takes_the_filters() {
        let theme = FileFilter::new("Theme", &["json"]);
        assert_eq!(
            with_default_extension(PathBuf::from("C:/out/nord"), &theme),
            PathBuf::from("C:/out/nord.json")
        );
        assert_eq!(
            with_default_extension(PathBuf::from("C:/out/nord.txt"), &theme),
            PathBuf::from("C:/out/nord.txt"),
            "a chosen extension stays"
        );
        assert_eq!(
            with_default_extension(
                PathBuf::from("C:/out/nord"),
                &FileFilter::new("Logs", &[".log"])
            ),
            PathBuf::from("C:/out/nord.log"),
            "a filter's dot is not doubled"
        );
        assert_eq!(
            with_default_extension(PathBuf::from("C:/out/nord"), &FileFilter::new("Any", &[])),
            PathBuf::from("C:/out/nord"),
            "a filter of every file adds none"
        );
    }

    /// A hang guard passed as the check's wait: a passing check answers long
    /// before it.
    const HANG_GUARD: Duration = Duration::from_secs(120);

    #[test]
    fn filter_spec_without_extensions_offers_every_file() {
        assert_eq!(
            filter_spec(&FileFilter::new("Any", &[])),
            ("Any".to_owned(), "*.*".to_owned())
        );
    }

    #[test]
    fn filter_spec_joins_the_extensions() {
        assert_eq!(
            filter_spec(&FileFilter::new("Prompts", &["txt", "md"])),
            ("Prompts".to_owned(), "*.txt;*.md".to_owned())
        );
    }

    #[test]
    fn filter_spec_drops_a_leading_dot() {
        assert_eq!(
            filter_spec(&FileFilter::new("Logs", &[".log"])),
            ("Logs".to_owned(), "*.log".to_owned())
        );
    }

    #[test]
    fn an_existing_folder_is_kept() {
        let dir = std::env::temp_dir();
        assert_eq!(
            start_folder_check(dir.clone(), HANG_GUARD, Path::is_dir),
            Some(dir)
        );
    }

    #[test]
    fn a_missing_folder_is_dropped() {
        let missing = std::env::temp_dir().join("rt-folder-picker-no-such-folder");
        assert_eq!(start_folder_check(missing, HANG_GUARD, Path::is_dir), None);
    }

    #[test]
    fn a_check_that_outlasts_its_wait_is_dropped() {
        // The probe answers only once the test has its result, so only the
        // check's own wait can end it.
        let (release, released) = std::sync::mpsc::channel::<()>();
        let stuck = move |_: &Path| {
            let _ = released.recv();
            true
        };
        let start = PathBuf::from("\\\\offline-host\\share");
        assert_eq!(
            start_folder_check(start, Duration::from_millis(20), stuck),
            None,
            "an unreachable share is not waited out"
        );
        drop(release);
    }
}
