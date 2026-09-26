//! The main window's size, position and monitor: what `native-ui.json` keeps
//! of them and how the window opens from it.

use gpui::{Bounds, Pixels, Size, Window, WindowBounds, point, px, size};
use serde::{Deserialize, Serialize};

/// The main window's place as saved: its restore rect in logical pixels,
/// whether it was maximized, and the monitor it was on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    /// The monitor's stable id; `None` when it could not be read.
    #[serde(default)]
    pub display_uuid: Option<String>,
}

/// Where the window's rect comes from when it opens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RestoreRect {
    /// Centred on the primary monitor at this size.
    Centered(Size<Pixels>),
    /// Exactly this rect.
    At(Bounds<Pixels>),
}

/// How the main window opens: on which monitor (`None` is the primary),
/// where, and whether maximized.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Restore<D> {
    pub display: Option<D>,
    pub rect: RestoreRect,
    pub maximized: bool,
}

/// What to save for a window whose bounds are `bounds`, on the monitor
/// `display_uuid`: its restore rect, maximized or not. A fullscreen window
/// saves its restore rect as a plain window. `None` for a rect with no area,
/// which is nothing to reopen at.
#[must_use]
pub fn to_window_state(bounds: WindowBounds, display_uuid: Option<String>) -> Option<WindowState> {
    let (rect, maximized) = match bounds {
        WindowBounds::Windowed(rect) | WindowBounds::Fullscreen(rect) => (rect, false),
        WindowBounds::Maximized(rect) => (rect, true),
    };
    let width = f32::from(rect.size.width);
    let height = f32::from(rect.size.height);
    let has_area = width > 0.0 && height > 0.0;
    has_area.then(|| WindowState {
        x: f32::from(rect.origin.x),
        y: f32::from(rect.origin.y),
        width,
        height,
        maximized,
        display_uuid,
    })
}

/// How to open the window from `saved`, given the connected monitors as
/// (id, uuid). Nothing saved opens `default_size` centred on the primary; a
/// saved monitor still connected gets the saved rect on it; one that is gone
/// gets the saved size centred on the primary. A rect saved without a
/// monitor is placed as saved, where gpui moves it onto the primary when it
/// is not on it.
#[must_use]
pub fn restore_options<D: Copy>(
    saved: Option<&WindowState>,
    displays: &[(D, String)],
    default_size: Size<Pixels>,
) -> Restore<D> {
    let Some(saved) = saved else {
        return Restore {
            display: None,
            rect: RestoreRect::Centered(default_size),
            maximized: false,
        };
    };
    let rect = Bounds {
        origin: point(px(saved.x), px(saved.y)),
        size: size(px(saved.width), px(saved.height)),
    };
    let (display, rect) = match &saved.display_uuid {
        None => (None, RestoreRect::At(rect)),
        Some(uuid) => displays
            .iter()
            .find(|(_, candidate)| candidate == uuid)
            .map_or((None, RestoreRect::Centered(rect.size)), |(id, _)| {
                (Some(*id), RestoreRect::At(rect))
            }),
    };
    Restore {
        display,
        rect,
        maximized: saved.maximized,
    }
}

/// Whether `window` is minimized, when its bounds say nothing about where it
/// should reopen. gpui has no such query, so this asks Windows. Only for a
/// real window: gpui's test windows panic when asked for their handle.
#[cfg(windows)]
#[must_use]
pub fn is_minimized(window: &Window) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsIconic;
    // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
    let raw = HasWindowHandle::window_handle(window).map(|handle| handle.as_raw());
    let handle = match raw {
        Ok(RawWindowHandle::Win32(handle)) => handle,
        other => {
            tracing::debug!("the window has no Win32 handle ({other:?}); taken as not minimized");
            return false;
        }
    };
    let address = handle.hwnd.get().cast_unsigned();
    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(address));
    // SAFETY: a plain query on this live window's handle.
    unsafe { IsIconic(hwnd) }.as_bool()
}

/// Whether `window` is minimized; only Windows is asked.
#[cfg(not(windows))]
#[must_use]
pub fn is_minimized(_window: &Window) -> bool {
    false
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::sidebar::UiState;

    fn default_size() -> Size<Pixels> {
        size(px(1000.0), px(640.0))
    }

    fn saved(uuid: Option<&str>, maximized: bool) -> WindowState {
        WindowState {
            x: 1920.0,
            y: 100.0,
            width: 1200.0,
            height: 800.0,
            maximized,
            display_uuid: uuid.map(str::to_owned),
        }
    }

    fn saved_rect() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(1920.0), px(100.0)),
            size: size(px(1200.0), px(800.0)),
        }
    }

    fn displays() -> Vec<(u32, String)> {
        vec![(0, "primary".to_owned()), (1, "secondary".to_owned())]
    }

    #[test]
    fn no_saved_state_opens_centred_default() {
        let restore = restore_options(None, &displays(), default_size());
        assert_eq!(
            restore,
            Restore {
                display: None,
                rect: RestoreRect::Centered(default_size()),
                maximized: false,
            }
        );
    }

    #[test]
    fn saved_display_is_resolved_by_uuid() {
        let state = saved(Some("secondary"), false);
        let restore = restore_options(Some(&state), &displays(), default_size());
        assert_eq!(
            restore,
            Restore {
                display: Some(1),
                rect: RestoreRect::At(saved_rect()),
                maximized: false,
            }
        );
    }

    #[test]
    fn missing_display_falls_back_to_primary_with_saved_size() {
        let state = saved(Some("unplugged"), false);
        let restore = restore_options(Some(&state), &displays(), default_size());
        assert_eq!(
            restore,
            Restore {
                display: None,
                rect: RestoreRect::Centered(saved_rect().size),
                maximized: false,
            }
        );
    }

    #[test]
    fn maximized_restores_maximized_with_its_restore_rect() {
        let state = saved(Some("secondary"), true);
        let restore = restore_options(Some(&state), &displays(), default_size());
        assert_eq!(
            restore,
            Restore {
                display: Some(1),
                rect: RestoreRect::At(saved_rect()),
                maximized: true,
            }
        );
        let back = to_window_state(
            WindowBounds::Maximized(saved_rect()),
            state.display_uuid.clone(),
        );
        assert_eq!(
            back,
            Some(state),
            "a maximized window saves its restore rect"
        );
    }

    #[test]
    fn fullscreen_saves_its_restore_rect_as_a_plain_window() {
        let state = to_window_state(WindowBounds::Fullscreen(saved_rect()), None);
        assert_eq!(state, Some(saved(None, false)));
    }

    #[test]
    fn degenerate_rect_is_not_saved() {
        let flat = Bounds {
            origin: point(px(10.0), px(10.0)),
            size: size(px(800.0), px(0.0)),
        };
        assert_eq!(to_window_state(WindowBounds::Windowed(flat), None), None);
        let negative = Bounds {
            origin: point(px(10.0), px(10.0)),
            size: size(px(-5.0), px(600.0)),
        };
        assert_eq!(
            to_window_state(WindowBounds::Windowed(negative), None),
            None
        );
    }

    #[test]
    fn window_state_round_trips_through_ui_json() {
        let ui = UiState {
            window: Some(saved(Some("secondary"), true)),
            ..UiState::default()
        };
        let text = serde_json::to_string(&ui).expect("serialize");
        let back: UiState = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, ui);

        let old: UiState =
            serde_json::from_str(r#"{ "sidebar_collapsed": true }"#).expect("an older file");
        assert_eq!(old.window, None, "a file from before window state loads");
    }
}
