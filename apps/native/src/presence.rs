//! Whether the owner is at the desk, as the client reports it to each daemon
//! that lists [`protocol::FEATURE_CLIENT_PRESENCE`].
//!
//! The owner counts as present while the main window is active and the
//! machine had keyboard or mouse input in the last [`IDLE_LIMIT`]. The idle
//! time is read on Windows and macOS; where it cannot be read, the window's
//! focus alone decides.

use std::time::Duration;

/// How long the machine may go without keyboard or mouse input before the
/// owner counts as away, even with the window active.
pub(crate) const IDLE_LIMIT: Duration = Duration::from_secs(180);

/// How often the answer is read again, so that walking away from an active
/// window flips it.
pub(crate) const RECHECK_EVERY: Duration = Duration::from_secs(10);

/// Whether the owner is at the desk: the window is `focused` and the machine's
/// `idle` time, when it could be read, is under [`IDLE_LIMIT`].
pub(crate) fn is_active(focused: bool, idle: Option<Duration>) -> bool {
    focused && idle.is_none_or(|idle| idle < IDLE_LIMIT)
}

/// The answer last sent to the hosts, so an unchanged one is not sent again.
#[derive(Debug, Default)]
pub(crate) struct PresenceReport {
    last: Option<bool>,
}

impl PresenceReport {
    /// `Some(active)` when it differs from the answer last sent, which it then
    /// becomes; `None` when it is the same.
    pub(crate) fn next(&mut self, active: bool) -> Option<bool> {
        if self.last == Some(active) {
            return None;
        }
        self.last = Some(active);
        Some(active)
    }
}

/// Reads how long ago the machine last had keyboard or mouse input: `None`
/// when it cannot be read.
pub type IdleReader = fn() -> Option<Duration>;

/// The idle reader the presence report uses.
struct SystemIdleReader(IdleReader);

impl gpui::Global for SystemIdleReader {}

/// Makes `reader` the idle reader the presence report uses; the UI specs
/// install one that reports input just now, so they never see the
/// developer's own idle time.
pub fn set_idle_reader(cx: &mut gpui::App, reader: IdleReader) {
    cx.set_global(SystemIdleReader(reader));
}

/// The idle reader in use: the one [`set_idle_reader`] installed, else
/// [`system_idle`] on Windows and macOS and none elsewhere.
pub(crate) fn idle_reader(cx: &gpui::App) -> IdleReader {
    cx.try_global::<SystemIdleReader>()
        .map_or(DEFAULT_READER, |reader| reader.0)
}

/// Windows says when the last keyboard or mouse input came.
#[cfg(windows)]
const DEFAULT_READER: IdleReader = system_idle;
/// macOS says how long ago the last input event came.
#[cfg(target_os = "macos")]
const DEFAULT_READER: IdleReader = system_idle;
/// Elsewhere the idle time is not read, so focus alone decides.
#[cfg(not(any(windows, target_os = "macos")))]
const DEFAULT_READER: IdleReader = || None;

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
}

/// `kCGEventSourceStateHIDSystemState`: the input the hardware delivered.
#[cfg(target_os = "macos")]
const HID_SYSTEM_STATE: i32 = 1;
/// `kCGAnyInputEventType`: any keyboard or mouse event.
#[cfg(target_os = "macos")]
const ANY_INPUT_EVENT: u32 = u32::MAX;

/// How long ago the machine last had keyboard or mouse input, by
/// `CGEventSourceSecondsSinceLastEventType`; `None` when its answer is not
/// a finite, non-negative number of seconds.
#[cfg(target_os = "macos")]
fn system_idle() -> Option<Duration> {
    // SAFETY: the function takes two plain integers, reads no memory of
    // ours and only answers a number of seconds.
    let seconds =
        unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, ANY_INPUT_EVENT) };
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

/// How long ago the session last had keyboard or mouse input, by
/// `GetLastInputInfo` against `GetTickCount` (both in milliseconds since
/// boot, wrapping after 49.7 days); `None` when the read fails.
#[cfg(windows)]
fn system_idle() -> Option<Duration> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: u32::try_from(size_of::<LASTINPUTINFO>()).ok()?,
        dwTime: 0,
    };
    // SAFETY: `info` is a live LASTINPUTINFO whose `cbSize` is its own size.
    if !unsafe { GetLastInputInfo(&raw mut info) }.as_bool() {
        return None;
    }
    // SAFETY: GetTickCount takes no arguments and cannot fail.
    let now = unsafe { GetTickCount() };
    Some(Duration::from_millis(u64::from(
        now.wrapping_sub(info.dwTime),
    )))
}

#[cfg(test)]
mod tests {
    use super::{IDLE_LIMIT, PresenceReport, is_active};
    use std::time::Duration;

    #[test]
    fn focused_with_input_just_now_is_active() {
        assert!(is_active(true, Some(Duration::ZERO)));
    }

    #[test]
    fn focused_with_input_179_seconds_ago_is_active() {
        assert!(is_active(true, Some(Duration::from_secs(179))));
    }

    #[test]
    fn focused_with_no_input_for_the_limit_is_away() {
        assert_eq!(IDLE_LIMIT, Duration::from_secs(180));
        assert!(!is_active(true, Some(Duration::from_secs(180))));
    }

    #[test]
    fn focused_with_an_unknown_idle_time_is_active() {
        assert!(is_active(true, None));
    }

    #[test]
    fn not_focused_is_away_even_with_input_just_now() {
        assert!(!is_active(false, Some(Duration::ZERO)));
    }

    #[test]
    fn the_first_answer_is_sent() {
        assert_eq!(PresenceReport::default().next(false), Some(false));
        assert_eq!(PresenceReport::default().next(true), Some(true));
    }

    #[test]
    fn an_unchanged_answer_is_not_sent_again() {
        let mut report = PresenceReport::default();
        assert_eq!(report.next(true), Some(true));
        assert_eq!(report.next(true), None);
    }

    #[test]
    fn a_changed_answer_is_sent() {
        let mut report = PresenceReport::default();
        assert_eq!(report.next(true), Some(true));
        assert_eq!(report.next(false), Some(false));
        assert_eq!(report.next(true), Some(true));
    }
}
