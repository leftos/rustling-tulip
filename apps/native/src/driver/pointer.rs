//! Pointer input as the window messages a real mouse sends, posted to the
//! window's own handle: gpui's own event dispatch cannot be called from
//! outside it.
//!
//! The pure parts build each gesture's messages from client pixels; on
//! Windows, [`post`] queues them on the window, behind whatever the window
//! already has to handle.

use crate::driver::protocol::Button;

const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MOUSEWHEEL: u32 = 0x020A;
/// A mouse message's `wParam` bit while the left button is down.
const MK_LBUTTON: usize = 0x0001;
/// A mouse message's `wParam` bit while the right button is down.
const MK_RBUTTON: usize = 0x0002;
/// One wheel notch's delta.
const WHEEL_DELTA: i32 = 120;

/// The distance, in logical pixels, a press must travel before the sidebar
/// divider counts it as a drag (`DIVIDER_DRAG_THRESHOLD`): each move of a
/// drag goes further than this when the distance allows.
pub const DRAG_THRESHOLD: f32 = 3.0;

/// One window message to post.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Posted {
    pub message: u32,
    pub wparam: usize,
    pub lparam: isize,
}

/// A point in logical window pixels in physical client pixels at `scale`,
/// rounded to the nearest.
#[must_use]
pub fn physical((x, y): (f32, f32), scale: f32) -> (i32, i32) {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a point inside the window is far inside i32; rounding is the intent"
    )]
    let round = |value: f32| (value * scale).round() as i32;
    (round(x), round(y))
}

/// The low 16 bits of `value`, as `LOWORD` takes them.
fn low_word(value: i32) -> usize {
    let [low, high, ..] = value.to_le_bytes();
    usize::from(u16::from_le_bytes([low, high]))
}

/// A point packed as `MAKELPARAM(x, y)`: each coordinate a signed 16-bit
/// word, `x` low, `y` high, as `GET_X_LPARAM` and `GET_Y_LPARAM` read them.
#[must_use]
pub fn lparam((x, y): (i32, i32)) -> isize {
    ((low_word(y) << 16) | low_word(x)).cast_signed()
}

/// `WM_MOUSEWHEEL`'s `wParam` for `notches`: the delta, 120 a notch, in the
/// high word, positive away from the user, and no keys held.
#[must_use]
pub fn wheel_wparam(notches: i32) -> usize {
    low_word(notches.saturating_mul(WHEEL_DELTA)) << 16
}

/// The moves of a drag from `from` to `to`, in logical pixels, after the
/// press at `from`: `steps` evenly spaced points, fewer when that many
/// would not each move further than [`DRAG_THRESHOLD`], the last exactly
/// `to`.
#[must_use]
pub fn drag_path(from: (f32, f32), to: (f32, f32), steps: u8) -> Vec<(f32, f32)> {
    let distance = (to.0 - from.0).hypot(to.1 - from.1);
    let mut count = steps.max(1);
    while count > 1 && distance / f32::from(count) <= DRAG_THRESHOLD {
        count -= 1;
    }
    (1..=count)
        .map(|step| {
            if step == count {
                return to;
            }
            let along = f32::from(step) / f32::from(count);
            (
                from.0 + (to.0 - from.0) * along,
                from.1 + (to.1 - from.1) * along,
            )
        })
        .collect()
}

fn message(message: u32, wparam: usize, at: (i32, i32)) -> Posted {
    Posted {
        message,
        wparam,
        lparam: lparam(at),
    }
}

/// A move to `at`, client pixels, with no button down.
#[must_use]
pub fn move_messages(at: (i32, i32)) -> Vec<Posted> {
    vec![message(WM_MOUSEMOVE, 0, at)]
}

/// `count` clicks of `button` at `at`, each a move, a press and a release:
/// a press after the move's `WM_MOUSELEAVE` would have no pointer over it.
#[must_use]
pub fn click_messages(at: (i32, i32), button: Button, count: u8) -> Vec<Posted> {
    let (down, held, up) = match button {
        Button::Left => (WM_LBUTTONDOWN, MK_LBUTTON, WM_LBUTTONUP),
        Button::Right => (WM_RBUTTONDOWN, MK_RBUTTON, WM_RBUTTONUP),
    };
    (0..count)
        .flat_map(|_| {
            [
                message(WM_MOUSEMOVE, 0, at),
                message(down, held, at),
                message(up, 0, at),
            ]
        })
        .collect()
}

/// A left-button drag: a move to `from`, the press there, a held move to
/// each point of `path`, and the release at its last point.
#[must_use]
pub fn drag_messages(from: (i32, i32), path: &[(i32, i32)]) -> Vec<Posted> {
    let end = path.last().copied().unwrap_or(from);
    let mut messages = vec![
        message(WM_MOUSEMOVE, 0, from),
        message(WM_LBUTTONDOWN, MK_LBUTTON, from),
    ];
    messages.extend(path.iter().map(|&at| message(WM_MOUSEMOVE, MK_LBUTTON, at)));
    messages.push(message(WM_LBUTTONUP, 0, end));
    messages
}

/// A move to `client`, then `notches` of the wheel there: `WM_MOUSEWHEEL`
/// carries the point in screen pixels, `screen`.
#[must_use]
pub fn wheel_messages(client: (i32, i32), screen: (i32, i32), notches: i32) -> Vec<Posted> {
    vec![
        message(WM_MOUSEMOVE, 0, client),
        Posted {
            message: WM_MOUSEWHEEL,
            wparam: wheel_wparam(notches),
            lparam: lparam(screen),
        },
    ]
}

/// The address of `window`'s Win32 handle, read fresh, which crosses
/// threads where an `HWND` cannot.
///
/// # Errors
///
/// When the window has no Win32 handle.
#[cfg(windows)]
pub(crate) fn window_address(window: &gpui::Window) -> Result<usize, String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
    match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => Ok(handle.hwnd.get().cast_unsigned()),
        other => Err(format!("the window has no Win32 handle ({other:?})")),
    }
}

/// Client pixel `at` of the window at `address` in screen pixels.
///
/// # Errors
///
/// When Windows will not convert it.
#[cfg(windows)]
pub fn client_to_screen(address: usize, (x, y): (i32, i32)) -> Result<(i32, i32), String> {
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::ClientToScreen;

    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(address));
    let mut point = POINT { x, y };
    // SAFETY: a plain conversion on this process's own window, into a local.
    if unsafe { ClientToScreen(hwnd, &raw mut point) }.as_bool() {
        Ok((point.x, point.y))
    } else {
        Err(format!("ClientToScreen refused the point ({x}, {y})"))
    }
}

/// Posts `messages`, in order, to the window at `address`.
///
/// # Errors
///
/// Which message Windows would not queue, and why; the ones before it
/// were posted.
#[cfg(windows)]
pub fn post(address: usize, messages: &[Posted]) -> Result<(), String> {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(address));
    for posted in messages {
        // SAFETY: queues a plain mouse message on this process's own window.
        unsafe {
            PostMessageW(
                Some(hwnd),
                posted.message,
                WPARAM(posted.wparam),
                LPARAM(posted.lparam),
            )
        }
        .map_err(|err| format!("posting message {:#06x}: {err}", posted.message))?;
    }
    Ok(())
}

/// Posted input needs a Win32 window.
#[cfg(not(windows))]
pub(crate) fn window_address(_window: &gpui::Window) -> Result<usize, String> {
    Err("posted pointer input needs a Win32 window".to_owned())
}

#[cfg(not(windows))]
pub fn client_to_screen(_address: usize, _at: (i32, i32)) -> Result<(i32, i32), String> {
    Err("posted pointer input needs a Win32 window".to_owned())
}

#[cfg(not(windows))]
pub fn post(_address: usize, _messages: &[Posted]) -> Result<(), String> {
    Err("posted pointer input needs a Win32 window".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{DRAG_THRESHOLD, drag_path, lparam, physical, wheel_wparam};

    #[test]
    fn physical_point_rounds_at_the_scale() {
        for (scale, expected) in [
            (1.0, (10, 21)),
            (1.25, (13, 26)),
            (1.5, (15, 31)),
            (2.0, (20, 41)),
        ] {
            assert_eq!(physical((10.0, 20.5), scale), expected, "scale {scale}");
        }
        assert_eq!(physical((0.3, 0.2), 1.25), (0, 0), "below half a pixel");
    }

    #[test]
    fn lparam_packs_signed_words() {
        assert_eq!(lparam((10, 20)), 0x0014_000A);
        assert_eq!(lparam((0, 0)), 0);
        assert_eq!(lparam((-5, 10)), 0x000A_FFFB, "x is a signed low word");
        assert_eq!(lparam((7, -1)), 0xFFFF_0007, "y is a signed high word");
    }

    #[test]
    fn wheel_wparam_encodes_notches() {
        assert_eq!(wheel_wparam(1), 0x0078_0000, "one notch up is +120");
        assert_eq!(
            wheel_wparam(-3),
            0xFE98_0000,
            "three notches down is -360 as a signed word"
        );
    }

    #[test]
    fn drag_steps_end_at_the_target_and_exceed_the_threshold() {
        for (from, to, steps, expected) in [
            ((100.0, 50.0), (180.0, 50.0), 8, 8),
            ((0.0, 0.0), (10.0, 0.0), 8, 3),
            ((5.0, 5.0), (5.0, 7.0), 8, 1),
            ((0.0, 0.0), (300.0, 400.0), 64, 64),
        ] {
            let path = drag_path(from, to, steps);
            assert_eq!(path.len(), expected, "{from:?} -> {to:?} in {steps}");
            assert_eq!(path.last(), Some(&to), "the last move lands on the target");
            let distance = (to.0 - from.0).hypot(to.1 - from.1);
            if distance > DRAG_THRESHOLD {
                let mut last = from;
                for &at in &path {
                    let moved = (at.0 - last.0).hypot(at.1 - last.1);
                    assert!(
                        moved > DRAG_THRESHOLD,
                        "{from:?} -> {to:?}: a {moved} px move is under the threshold"
                    );
                    last = at;
                }
            }
        }
    }
}
