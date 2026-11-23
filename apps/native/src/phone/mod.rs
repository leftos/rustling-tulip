//! The phone app's shell: one full-screen window padded clear of the notch
//! and the home indicator, drawn by [`PhoneRoot`] over a phone-shape
//! [`RootView`].

mod autoscroll;
pub(crate) mod browser;
mod browser_view;
mod composer;
mod fetched;
mod home;
mod home_view;
mod hosts_view;
#[cfg(target_os = "ios")]
mod idiom_ios;
mod image_zoom;
mod key_bar;
pub mod key_grid;
#[cfg(target_os = "ios")]
mod keychain_selftest;
mod keys_view;
mod layout;
pub(crate) mod lifecycle;
mod long_press;
#[cfg(target_os = "ios")]
mod open_ios;
mod open_tap;
pub(crate) mod order;
#[cfg(target_os = "ios")]
mod paste_control_ios;
pub mod pasteboard;
#[cfg(target_os = "ios")]
mod pasteboard_ios;
pub mod photos;
#[cfg(target_os = "ios")]
mod photos_ios;
mod prompt_sheet;
pub mod push;
#[cfg(target_os = "ios")]
mod push_ios;
pub mod quick_replies;
mod quick_replies_view;
mod reorder;
mod root_view;
mod session;
mod session_view;
mod settings;
mod settings_view;
pub mod share;
#[cfg(target_os = "ios")]
mod share_ios;
pub(crate) mod sheet;
mod sweep;
mod terminal_fit;
mod terminal_view;
mod tray;
mod viewer_view;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{App, AppContext as _, Edges, Pixels, WindowInsets, WindowOptions};

use crate::open::Opener;
use crate::{
    LiveSeams, NoTaskbarBadge, NoToastPlacer, RootDeps, RootShape, RootView, SilentNotifier,
    SilentSounder,
};
pub use layout::PhoneLayout;
use pasteboard::PhonePasteboard;
use photos::PhotoPicker;
use push::PushPlatform;
pub use root_view::PhoneRoot;
use share::ShareSheet;

/// Starts the phone app: logging to stderr, then `register_delegates` (which
/// registers the platform's app delegate classes before the app starts, so the
/// background-push method can be added to them first), then one full-screen
/// window. `on_start` runs first inside the app, before the window opens.
pub fn run(register_delegates: impl FnOnce(), on_start: impl FnOnce(&mut App) + 'static) {
    // Another subscriber already set is the only error, and it still logs.
    let _already_set = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    register_delegates();
    #[cfg(target_os = "ios")]
    push_ios::add_wake_method();
    #[cfg(target_os = "ios")]
    push_ios::add_tap_delegate();
    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .run(move |cx: &mut App| {
            on_start(cx);
            crate::bind_keys(cx);
            crate::fonts::register_bundled(cx);
            let platform = push_platform(cx);
            let share = share_sheet();
            let root_share = Arc::clone(&share);
            let opened = cx.open_window(WindowOptions::default(), |window, cx| {
                let root = cx.new(|cx| RootView::with_transport(phone_deps(), window, cx));
                cx.new(|cx| {
                    PhoneRoot::new(root, platform, window, cx)
                        .with_share(root_share)
                        .with_pasteboard(phone_pasteboard())
                        .with_photos(photo_picker())
                })
            });
            if let Err(err) = opened {
                tracing::error!(error = %err, "phone window failed to open");
                cx.quit();
                return;
            }
            cx.activate(true);
            #[cfg(target_os = "ios")]
            share_selftest(share, cx);
            #[cfg(target_os = "ios")]
            open_selftest(cx);
            #[cfg(target_os = "ios")]
            keychain_selftest();
        });
}

/// The phone's opener on iOS: a web link opens in the system, the rest only
/// logs.
#[cfg(target_os = "ios")]
fn phone_opener() -> Arc<dyn Opener> {
    Arc::new(open_ios::IosOpener)
}

/// Off iOS the phone opens nothing: every open only logs.
#[cfg(not(target_os = "ios"))]
fn phone_opener() -> Arc<dyn Opener> {
    crate::open::opener_for(true)
}

/// Whether the phone opens `url` itself: an `http` or `https` address, in
/// any case, with no whitespace or control characters and something after
/// its `://`.
#[cfg(any(target_os = "ios", test))]
fn is_web_link(url: &str) -> bool {
    crate::open::validate_http_url(url).is_ok()
        && url
            .split_once("://")
            .is_some_and(|(_, rest)| !rest.is_empty() && !rest.starts_with('/'))
}

/// `url` with every byte iOS 15's `NSURL` refuses percent-encoded as `%XX`
/// in upper-case hex: each non-ASCII byte of its UTF-8, each ASCII byte in
/// neither RFC 3986's unreserved nor its reserved set, and `[` and `]` after
/// the authority (inside it they bracket an IPv6 host). A `%` is left as it
/// is, so an `%XX` already there is never encoded again.
#[cfg(any(target_os = "ios", test))]
fn percent_encode_link(url: &str) -> String {
    const RESERVED: &[u8] = b":/?#[]@!$&'()*+,;=";
    let authority_start = url.find("://").map_or(0, |at| at + 3);
    let path_start = url[authority_start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |at| authority_start + at);
    let mut encoded = String::with_capacity(url.len());
    for (at, byte) in url.bytes().enumerate() {
        let bracket = byte == b'[' || byte == b']';
        let kept = byte.is_ascii_alphanumeric()
            || b"-._~%".contains(&byte)
            || (RESERVED.contains(&byte) && !(bracket && at >= path_start));
        if kept {
            encoded.push(char::from(byte));
        } else {
            encoded.extend(['%', upper_hex(byte >> 4), upper_hex(byte & 0x0F)]);
        }
    }
    encoded
}

/// The upper-case hex digit of `nibble`, a value below 16.
#[cfg(any(target_os = "ios", test))]
fn upper_hex(nibble: u8) -> char {
    char::from(if nibble < 10 {
        b'0' + nibble
    } else {
        b'A' + nibble - 10
    })
}

/// The phone's share sheet on iOS: `UIActivityViewController`.
#[cfg(target_os = "ios")]
fn share_sheet() -> Arc<dyn ShareSheet> {
    Arc::new(share_ios::IosShareSheet)
}

/// No share sheet off iOS: a Share only logs.
#[cfg(not(target_os = "ios"))]
fn share_sheet() -> Arc<dyn ShareSheet> {
    Arc::new(share::LoggingShare)
}

/// The phone's pasteboard on iOS: `UIPasteboard`.
#[cfg(target_os = "ios")]
fn phone_pasteboard() -> Arc<dyn PhonePasteboard> {
    Arc::new(pasteboard_ios::IosPasteboard)
}

/// No pasteboard image off iOS: Paste pastes text.
#[cfg(not(target_os = "ios"))]
fn phone_pasteboard() -> Arc<dyn PhonePasteboard> {
    Arc::new(pasteboard::LoggingPasteboard)
}

/// The phone's photo picker on iOS: `PHPickerViewController`.
#[cfg(target_os = "ios")]
fn photo_picker() -> Arc<dyn PhotoPicker> {
    Arc::new(photos_ios::IosPhotoPicker)
}

/// No photo picker off iOS: Photos only logs.
#[cfg(not(target_os = "ios"))]
fn photo_picker() -> Arc<dyn PhotoPicker> {
    Arc::new(photos::LoggingPhotos)
}

/// Launched with [`share_ios::SHARE_SELFTEST_ARG`], presents the share sheet
/// for a small file it writes into the copies folder.
#[cfg(target_os = "ios")]
fn share_selftest(share: Arc<dyn ShareSheet>, cx: &mut App) {
    if std::env::args().any(|arg| arg == share_ios::SHARE_SELFTEST_ARG) {
        share_ios::start_selftest(share, cx);
    }
}

/// Launched with [`open_ios::OPEN_SELFTEST_ARG`], opens a web page once the
/// window shows.
#[cfg(target_os = "ios")]
fn open_selftest(cx: &mut App) {
    if std::env::args().any(|arg| arg == open_ios::OPEN_SELFTEST_ARG) {
        open_ios::start_selftest(cx);
    }
}

/// Launched with [`keychain_selftest::KEYCHAIN_SELFTEST_ARG`], saves, reads
/// back and removes a token in the device token store, off the main thread.
#[cfg(target_os = "ios")]
fn keychain_selftest() {
    if std::env::args().any(|arg| arg == keychain_selftest::KEYCHAIN_SELFTEST_ARG) {
        keychain_selftest::start();
    }
}

/// The phone's push on iOS; launched with
/// [`push_ios::PUSH_PROVISIONAL_ARG`] it asks for provisional notification
/// permission and the device token at once, else nothing until a switch asks.
#[cfg(target_os = "ios")]
fn push_platform(cx: &mut App) -> Arc<dyn PushPlatform> {
    push_ios::add_wake_method();
    let platform = Arc::new(push_ios::IosPushPlatform);
    if std::env::args().any(|arg| arg == push_ios::PUSH_PROVISIONAL_ARG) {
        push_ios::start_provisional(Arc::clone(&platform), cx);
    }
    platform
}

/// No push off iOS: the switch never draws.
#[cfg(not(target_os = "ios"))]
fn push_platform(_cx: &mut App) -> Arc<dyn PushPlatform> {
    Arc::new(push::UnsupportedPlatform)
}

/// What the phone's root view talks to: the live network thread, token
/// store and config dir, the desktop's notification, sound, toast and badge
/// seams silent, and an updater that never checks (a phone app does not
/// update itself).
fn phone_deps() -> RootDeps {
    let LiveSeams {
        open_host,
        ui_dir,
        paths,
        tokens,
    } = crate::live_seams();
    RootDeps {
        open_host,
        ui_dir,
        paths,
        wanted: None,
        now: Arc::new(Instant::now),
        quit: Box::new(|cx: &mut App| cx.quit()),
        open: phone_opener(),
        notify: Arc::new(SilentNotifier),
        toast_placer: Arc::new(NoToastPlacer),
        sound: Arc::new(SilentSounder),
        badge: Rc::new(NoTaskbarBadge),
        pick_folder: crate::folder_picker::offscreen_folder_picker(),
        pick_file: crate::folder_picker::offscreen_file_picker(),
        pick_save: crate::folder_picker::offscreen_save_picker(),
        autostart: crate::remote_access::autostart_for(true),
        tokens,
        updater: Arc::new(crate::update::SystemUpdater::new(
            daemon_client::updates_dir().map_err(|err| format!("{err:#}")),
            false,
        )),
        start_mode: crate::start_guard::StartMode::Normal,
        shape: RootShape::Phone,
    }
}

/// The padding that keeps content clear of the system's UI: the safe area,
/// which covers the notch, the status bar and the home indicator, with its
/// bottom raised to the keyboard's, so the content sits above the raised
/// keyboard too and drops back when the keyboard hides.
#[must_use]
pub fn content_padding(insets: &WindowInsets) -> Edges<Pixels> {
    let safe_area = insets.safe_area;
    Edges {
        bottom: safe_area.bottom.max(insets.ime.bottom),
        ..safe_area
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;

    #[test]
    fn the_phone_opens_web_links_only() {
        for url in [
            "https://example.com/",
            "http://example.com",
            "HTTPS://linear.app/rt/issue/RT-1",
        ] {
            assert!(is_web_link(url), "{url} is a web link");
        }
        let refused = [
            "linear://linear.app/rt/issue/RT-1",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "not a url",
            "https://",
            "https:///path",
            "https://exa mple.com",
        ];
        for url in refused {
            assert!(!is_web_link(url), "{url} is not a web link");
        }
    }

    #[test]
    fn a_link_is_percent_encoded_before_it_is_parsed() {
        let rows = [
            (
                "https://de.wikipedia.org/wiki/Köln",
                "https://de.wikipedia.org/wiki/K%C3%B6ln",
            ),
            ("https://x.test/?a[]=1", "https://x.test/?a%5B%5D=1"),
            ("https://x.test/a%3Ab", "https://x.test/a%3Ab"),
            ("https://[::1]:8080/p", "https://[::1]:8080/p"),
            (
                "https://example.com/a/b?c=d&e=f#g",
                "https://example.com/a/b?c=d&e=f#g",
            ),
        ];
        for (url, encoded) in rows {
            assert_eq!(percent_encode_link(url), encoded, "{url}");
        }
    }

    #[test]
    fn padding_is_the_safe_area() {
        let safe_area = Edges {
            top: px(62.0),
            bottom: px(34.0),
            ..Edges::default()
        };
        let insets = WindowInsets {
            safe_area,
            ..WindowInsets::default()
        };
        assert_eq!(content_padding(&insets), safe_area);
    }

    #[test]
    fn a_keyboard_taller_than_the_safe_area_raises_the_bottom() {
        let safe_area = Edges {
            top: px(62.0),
            right: px(4.0),
            bottom: px(34.0),
            left: px(4.0),
        };
        let insets = WindowInsets {
            safe_area,
            ime: Edges {
                bottom: px(336.0),
                ..Edges::default()
            },
            ..WindowInsets::default()
        };
        assert_eq!(
            content_padding(&insets),
            Edges {
                top: px(62.0),
                right: px(4.0),
                bottom: px(336.0),
                left: px(4.0),
            },
            "the keyboard raises the bottom alone"
        );
    }

    #[test]
    fn the_safe_area_wins_while_no_keyboard_shows() {
        let insets = WindowInsets {
            safe_area: Edges {
                top: px(62.0),
                bottom: px(34.0),
                ..Edges::default()
            },
            ..WindowInsets::default()
        };
        assert_eq!(
            content_padding(&insets).bottom,
            px(34.0),
            "the home indicator's band stays"
        );
    }

    #[test]
    fn a_keyboard_shorter_than_the_safe_area_keeps_the_safe_area() {
        let insets = WindowInsets {
            safe_area: Edges {
                bottom: px(34.0),
                ..Edges::default()
            },
            ime: Edges {
                bottom: px(20.0),
                ..Edges::default()
            },
            ..WindowInsets::default()
        };
        assert_eq!(
            content_padding(&insets).bottom,
            px(34.0),
            "the safe area covers the keyboard's band"
        );
    }

    #[test]
    fn no_insets_means_no_padding() {
        assert_eq!(
            content_padding(&WindowInsets::default()),
            Edges::<Pixels>::default()
        );
    }
}
