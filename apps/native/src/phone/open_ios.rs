//! The phone's opener on iOS: a web link opens through
//! `UIApplication.openURL:options:completionHandler:` on the main thread, so
//! Safari, or the app that claims the address through its universal link,
//! shows it. Everything else the phone cannot do on its own only logs, as the
//! cloaked window's opener does.
//!
//! Launched with [`OPEN_SELFTEST_ARG`], the app opens `https://example.com/`
//! once its window shows: the simulator's hands-off open check
//! (`tools/ios/sim-open.ps1`) passes it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use gpui::App;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2_foundation::{NSDictionary, NSString, NSURL};
use objc2_ui_kit::{UIApplication, UIApplicationOpenExternalURLOptionsKey};

use crate::open::{OpenFailure, Opener};

/// The launch argument that opens [`SELFTEST_URL`] once the window shows.
pub const OPEN_SELFTEST_ARG: &str = "--open-selftest";

/// The web page the self-test opens.
const SELFTEST_URL: &str = "https://example.com/";

/// How long the self-test waits for the window to be key and shown before it
/// opens the page.
const SELFTEST_DELAY: Duration = Duration::from_secs(2);

/// The iOS opener: web links open in the system, the rest only logs.
pub struct IosOpener;

impl IosOpener {
    /// The cloaked window's opener, which every open the phone cannot do
    /// itself goes to: it only logs what it would have opened.
    fn logging() -> Arc<dyn Opener> {
        crate::open::opener_for(true)
    }
}

impl Opener for IosOpener {
    fn url(&self, url: &str) -> Result<(), String> {
        if !super::is_web_link(url) {
            return Err(format!("the phone opens only web links, not {url}"));
        }
        if web_address(url).is_none() {
            return Err(format!("the phone could not read the link {url}"));
        }
        if let Some(mtm) = MainThreadMarker::new() {
            open_on_main(url, mtm);
            return Ok(());
        }
        let shown = url.to_owned();
        DispatchQueue::main().exec_async(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                open_on_main(&shown, mtm);
            } else {
                tracing::warn!("phone: the main queue ran off the main thread; {shown} not opened");
            }
        });
        Ok(())
    }

    fn scheme_registered(&self, _scheme: &str) -> bool {
        false
    }

    fn linear_app(&self, _url: &str) -> Result<(), String> {
        Err("the Linear app opens from its web link on the phone".to_owned())
    }

    fn vscode(&self, path: &Path, line: u32, column: u32) -> Result<(), OpenFailure> {
        Self::logging().vscode(path, line, column)
    }

    fn vscode_folder(&self, paths: &[PathBuf]) -> Result<(), OpenFailure> {
        Self::logging().vscode_folder(paths)
    }

    fn vscode_workspace(&self, path: &Path) -> Result<(), OpenFailure> {
        Self::logging().vscode_workspace(path)
    }

    fn default_app(&self, path: &Path) -> Result<(), String> {
        Self::logging().default_app(path)
    }

    fn page(&self, path: &Path, suffix: &str) -> Result<(), String> {
        Self::logging().page(path, suffix)
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        Self::logging().reveal(path)
    }

    fn mapped_unc_hosts(&self) -> Vec<String> {
        Self::logging().mapped_unc_hosts()
    }

    fn is_dangerous_type(&self, extension: &str) -> bool {
        Self::logging().is_dangerous_type(extension)
    }

    fn existing(&self, reading: &Path) -> Option<Result<PathBuf, String>> {
        Self::logging().existing(reading)
    }

    fn path_exists(&self, path: &Path) -> bool {
        Self::logging().path_exists(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        Self::logging().is_file(path)
    }
}

/// `url` as an `NSURL` when it is a web link: [`super::is_web_link`] passes
/// it, Foundation parses it once [`super::percent_encode_link`] has encoded
/// what iOS 15 refuses, and reads its scheme as `http` or `https`, in any
/// case.
fn web_address(url: &str) -> Option<Retained<NSURL>> {
    if !super::is_web_link(url) {
        return None;
    }
    let encoded = super::percent_encode_link(url);
    let address = NSURL::URLWithString(&NSString::from_str(&encoded))?;
    let scheme = address.scheme()?.to_string();
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")).then_some(address)
}

/// Hands the web link `url` to the system; the completion logs
/// `phone: url opened` with whether iOS opened it.
fn open_on_main(url: &str, mtm: MainThreadMarker) {
    let Some(address) = web_address(url) else {
        tracing::warn!("phone: the phone could not read the link {url}");
        return;
    };
    let options: Retained<NSDictionary<UIApplicationOpenExternalURLOptionsKey, AnyObject>> =
        NSDictionary::new();
    let shown = url.to_owned();
    let done = RcBlock::new(move |success: Bool| {
        let success = success.as_bool();
        tracing::info!(success, "phone: url opened");
        if !success {
            tracing::warn!("phone: iOS refused to open {shown}");
        }
    });
    // SAFETY: the options dictionary is empty, so no key or value can be of
    // the wrong type, and this runs on the main thread `mtm` proves.
    unsafe {
        UIApplication::sharedApplication(mtm).openURL_options_completionHandler(
            &address,
            &options,
            Some(&done),
        );
    }
}

/// With [`OPEN_SELFTEST_ARG`]: opens [`SELFTEST_URL`] once the window has had
/// [`SELFTEST_DELAY`] to show.
pub fn start_selftest(cx: &mut App) {
    tracing::info!("phone: open self-test started");
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SELFTEST_DELAY).await;
        if let Err(err) = IosOpener.url(SELFTEST_URL) {
            tracing::warn!(error = %err, "phone: the open self-test was refused");
        }
    })
    .detach();
}
