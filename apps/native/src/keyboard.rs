//! The app's one seam for the on-screen keyboard: the calls that raise and
//! lower it, the reading that says whether it is up, and the log of the
//! calls the UI specs assert against.

use std::cell::RefCell;
use std::collections::VecDeque;

use gpui::{Window, WindowInsets, px};

/// A raise or a lower the app asked the window for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardCall {
    /// The keyboard was asked to come up.
    Raise,
    /// The keyboard was asked to go away.
    Lower,
}

/// The most calls the log keeps.
const KEPT: usize = 32;

thread_local! {
    /// The keyboard calls this thread made, newest last, only their last
    /// [`KEPT`].
    static CALLS: RefCell<VecDeque<KeyboardCall>> = const { RefCell::new(VecDeque::new()) };
}

/// Raises the on-screen keyboard.
pub(crate) fn raise(window: &mut Window) {
    window.request_virtual_keyboard();
    log(KeyboardCall::Raise);
}

/// Lowers the on-screen keyboard, leaving the window's focus where it was.
pub(crate) fn lower(window: &mut Window) {
    window.dismiss_virtual_keyboard();
    log(KeyboardCall::Lower);
}

/// Whether the on-screen keyboard is up: insets whose bottom IME band has
/// height.
#[must_use]
pub fn is_up(insets: &WindowInsets) -> bool {
    insets.ime.bottom > px(0.0)
}

/// The last calls, oldest first, for the UI specs.
#[must_use]
pub fn keyboard_calls() -> Vec<KeyboardCall> {
    CALLS.with(|calls| calls.borrow().iter().copied().collect())
}

/// Forgets the calls logged so far.
pub fn clear_keyboard_calls() {
    CALLS.with(|calls| calls.borrow_mut().clear());
}

/// Appends `call`, dropping the oldest once the log is full.
fn log(call: KeyboardCall) {
    CALLS.with(|calls| {
        let mut calls = calls.borrow_mut();
        if calls.len() == KEPT {
            calls.pop_front();
        }
        calls.push_back(call);
    });
}

#[cfg(test)]
mod tests {
    use gpui::{Edges, WindowInsets, px};

    use super::{KEPT, KeyboardCall, clear_keyboard_calls, is_up, keyboard_calls, log};

    /// Insets whose keyboard is `keyboard` tall.
    fn insets(keyboard: f32) -> WindowInsets {
        WindowInsets {
            ime: Edges {
                bottom: px(keyboard),
                ..Edges::default()
            },
            ..WindowInsets::default()
        }
    }

    #[test]
    fn the_keyboard_is_up_only_with_a_bottom_ime_band() {
        assert!(!is_up(&insets(0.0)));
        assert!(is_up(&insets(336.0)));
    }

    #[test]
    fn the_log_keeps_the_last_calls_oldest_first() {
        clear_keyboard_calls();
        log(KeyboardCall::Raise);
        log(KeyboardCall::Lower);
        assert_eq!(keyboard_calls(), [KeyboardCall::Raise, KeyboardCall::Lower]);
        clear_keyboard_calls();
        assert_eq!(keyboard_calls(), []);
    }

    #[test]
    fn the_log_keeps_only_its_last_calls() {
        clear_keyboard_calls();
        for _ in 0..KEPT {
            log(KeyboardCall::Raise);
        }
        log(KeyboardCall::Lower);
        let calls = keyboard_calls();
        assert_eq!(calls.len(), KEPT);
        assert_eq!(calls.last(), Some(&KeyboardCall::Lower));
        clear_keyboard_calls();
    }
}
