//! The phone app's shell: one full-screen window padded clear of the notch
//! and the home indicator, drawn by [`PhoneRoot`] over a phone-shape
//! [`RootView`].

mod composer;
mod home;
mod home_view;
mod hosts_view;
mod key_bar;
pub(crate) mod lifecycle;
mod long_press;
mod open_tap;
mod prompt_sheet;
pub mod push;
#[cfg(target_os = "ios")]
mod push_ios;
mod root_view;
mod session;
mod session_view;
pub(crate) mod sheet;
mod sweep;
mod tray;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{App, AppContext as _, Edges, Pixels, WindowInsets, WindowOptions};

use crate::{
    LiveSeams, NoTaskbarBadge, NoToastPlacer, RootDeps, RootShape, RootView, SilentNotifier,
    SilentSounder,
};
use push::PushPlatform;
pub use root_view::PhoneRoot;

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
            let opened = cx.open_window(WindowOptions::default(), |window, cx| {
                let root = cx.new(|cx| RootView::with_transport(phone_deps(), window, cx));
                cx.new(|cx| PhoneRoot::new(root, platform, window, cx))
            });
            if let Err(err) = opened {
                tracing::error!(error = %err, "phone window failed to open");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
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
        open: crate::open::opener_for(true),
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
