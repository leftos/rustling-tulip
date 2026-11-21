//! The phone's share sheet on iOS: `UIActivityViewController` for a copy's
//! file URL, presented from the topmost view controller of the app's key
//! window on the main thread. On an iPad the sheet is a popover, anchored at
//! the top right of the presenting view so it always has a source.
//!
//! Launched with [`SHARE_SELFTEST_ARG`], the app writes a small file into the
//! copies folder and presents the sheet for it: the simulator's hands-off
//! share check (`tools/ios/sim-share.ps1`) passes it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use gpui::App;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly as _};
use objc2_foundation::{NSArray, NSString, NSURL};
use objc2_ui_kit::{UIActivityViewController, UIApplication, UIViewController};

use super::share::ShareSheet;

/// The launch argument that writes a small file into the copies folder and
/// presents the share sheet for it once the window shows.
pub const SHARE_SELFTEST_ARG: &str = "--share-selftest";

/// The self-test's file name in the copies folder.
const SELFTEST_FILE: &str = "share-selftest.txt";

/// How long the self-test waits for the window to be key and shown before it
/// presents the sheet.
const SELFTEST_DELAY: Duration = Duration::from_secs(2);

/// The iOS share sheet.
pub struct IosShareSheet;

impl ShareSheet for IosShareSheet {
    fn present(&self, path: &Path) {
        if let Some(mtm) = MainThreadMarker::new() {
            present_on_main(path, mtm);
            return;
        }
        let path = path.to_path_buf();
        DispatchQueue::main().exec_async(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                present_on_main(&path, mtm);
            } else {
                tracing::warn!("phone: the main queue ran off the main thread; no share sheet");
            }
        });
    }
}

/// Presents the share sheet for `path` from the topmost view controller;
/// logs `phone: share sheet presented <path>` once `UIKit` has shown it.
fn present_on_main(path: &Path, mtm: MainThreadMarker) {
    let Some(presenter) = top_view_controller(mtm) else {
        tracing::warn!(path = %path.display(), "phone: no view controller to present the share sheet from");
        return;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let item: Retained<AnyObject> = url.into();
    let items = NSArray::from_retained_slice(&[item]);
    // SAFETY: the activity items are one file URL, an item type the sheet
    // accepts, and no application activities are given.
    let sheet = unsafe {
        UIActivityViewController::initWithActivityItems_applicationActivities(
            UIActivityViewController::alloc(mtm),
            &items,
            None,
        )
    };
    if !anchor_popover(&sheet, &presenter) {
        tracing::warn!(path = %path.display(), "phone: the share popover has no view to anchor to");
        return;
    }
    let shown = path.display().to_string();
    let done = RcBlock::new(move || tracing::info!("phone: share sheet presented {shown}"));
    presenter.presentViewController_animated_completion(&sheet, true, Some(&done));
}

/// Anchors `sheet`'s popover, when it has one (an iPad), at the top right of
/// `presenter`'s view, as both a source view and a source rect, since a
/// popover with no source raises. Whether the sheet may be presented.
fn anchor_popover(sheet: &UIActivityViewController, presenter: &UIViewController) -> bool {
    let Some(popover) = sheet.popoverPresentationController() else {
        return true;
    };
    let Some(view) = presenter.view() else {
        return false;
    };
    let mut rect = view.bounds();
    rect.origin.x += rect.size.width;
    rect.size.width = 0.0;
    rect.size.height = 0.0;
    popover.setSourceView(Some(&view));
    popover.setSourceRect(rect);
    true
}

/// The view controller at the top of the key window's presentation chain,
/// so a sheet already up does not refuse the new one.
fn top_view_controller(mtm: MainThreadMarker) -> Option<Retained<UIViewController>> {
    #[expect(
        deprecated,
        reason = "the app does not hold its window scene; gpui_ios opens one window"
    )]
    let windows = UIApplication::sharedApplication(mtm).windows();
    let window = windows
        .iter()
        .find(|window| window.isKeyWindow())
        .or_else(|| windows.firstObject())?;
    let mut top = window.rootViewController()?;
    while let Some(presented) = top.presentedViewController() {
        top = presented;
    }
    Some(top)
}

/// With [`SHARE_SELFTEST_ARG`]: writes the self-test file into the copies
/// folder, then presents the share sheet for it once the window has had
/// [`SELFTEST_DELAY`] to show.
pub fn start_selftest(share: Arc<dyn ShareSheet>, cx: &mut App) {
    let path = match write_selftest_file() {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!(error = %err, "phone: the share self-test file could not be written");
            return;
        }
    };
    tracing::info!(path = %path.display(), "phone: share self-test file written");
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SELFTEST_DELAY).await;
        share.present(&path);
    })
    .detach();
}

/// Writes the self-test file into the copies folder and gives its path.
///
/// # Errors
///
/// The copies folder is unknown, or the folder or the file could not be
/// written.
fn write_selftest_file() -> Result<PathBuf, String> {
    let dir = daemon_client::remote_files_dir().map_err(|err| format!("{err:#}"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("the copies folder could not be made: {err}"))?;
    let path = dir.join(SELFTEST_FILE);
    std::fs::write(&path, "rustling-tulip share self-test\n")
        .map_err(|err| format!("the self-test file could not be written: {err}"))?;
    Ok(path)
}
