//! The phone's layout, picked from the window's usable size rather than the
//! device: one pane at a time, Home's list beside a session, or a terminal
//! that fills the screen.

use gpui::{Pixels, Size, WindowInsets};

/// The narrowest usable width, in points, that fits two panes.
const TWO_PANE_WIDTH: f32 = 700.0;

/// The shortest usable height, in points, that keeps a session's header: a
/// shorter one gives the terminal the whole screen.
const TERMINAL_FULL_HEIGHT: f32 = 500.0;

/// How the phone lays its content out, picked from the window's usable size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhoneLayout {
    /// One pane at a time, as the phone in portrait draws it: Home fills the
    /// window, a session's page replaces it, and the session's terminal
    /// fills the window over it.
    Single,
    /// Home's list docks on the left with the selected session's page on the
    /// right, the Terminal toggle switching that pane's page: the iPad.
    TwoPane,
    /// Home and a session's page lay out as two panes, and a session's
    /// Terminal page fills the screen: the iPhone on its side. Turning it
    /// on its side moves a session with a terminal from its Chat page to
    /// it, the Terminal toggle in the two panes opens it, and a session with
    /// no terminal stays on its Chat page in the session pane.
    TerminalFull,
}

impl PhoneLayout {
    /// The layout for a `viewport` with `insets` drawn around it: the usable
    /// size is the viewport less the safe area, and the keyboard is not
    /// counted.
    #[must_use]
    pub fn of(viewport: Size<Pixels>, insets: &WindowInsets) -> Self {
        let safe = insets.safe_area;
        let width = f32::from(viewport.width) - f32::from(safe.left) - f32::from(safe.right);
        let height = f32::from(viewport.height) - f32::from(safe.top) - f32::from(safe.bottom);
        if height < TERMINAL_FULL_HEIGHT {
            Self::TerminalFull
        } else if width >= TWO_PANE_WIDTH {
            Self::TwoPane
        } else {
            Self::Single
        }
    }

    /// Whether Home's list and the selected session lay out side by side:
    /// the iPad's two panes, and the iPhone on its side.
    #[must_use]
    pub fn two_pane(self) -> bool {
        matches!(self, Self::TwoPane | Self::TerminalFull)
    }
}

#[cfg(test)]
mod tests {
    use super::PhoneLayout;
    use gpui::{Edges, Pixels, Size, WindowInsets, px};

    fn viewport(width: f32, height: f32) -> Size<Pixels> {
        Size {
            width: px(width),
            height: px(height),
        }
    }

    fn safe_area(top: f32, right: f32, bottom: f32, left: f32) -> WindowInsets {
        WindowInsets {
            safe_area: Edges {
                top: px(top),
                right: px(right),
                bottom: px(bottom),
                left: px(left),
            },
            ..WindowInsets::default()
        }
    }

    fn layout(width: f32, height: f32, insets: &WindowInsets) -> PhoneLayout {
        PhoneLayout::of(viewport(width, height), insets)
    }

    #[test]
    fn two_panes_are_the_ipads_and_the_iphone_on_its_side() {
        assert!(PhoneLayout::TwoPane.two_pane(), "the iPad's layout");
        assert!(
            PhoneLayout::TerminalFull.two_pane(),
            "the iPhone on its side lays Home out as two panes"
        );
        assert!(!PhoneLayout::Single.two_pane(), "the phone in portrait");
    }

    #[test]
    fn an_iphone_in_portrait_is_single() {
        assert_eq!(
            layout(402.0, 874.0, &safe_area(62.0, 0.0, 34.0, 0.0)),
            PhoneLayout::Single
        );
    }

    #[test]
    fn an_iphone_on_its_side_fills_the_terminal() {
        assert_eq!(
            layout(874.0, 402.0, &safe_area(0.0, 62.0, 21.0, 62.0)),
            PhoneLayout::TerminalFull
        );
    }

    #[test]
    fn an_ipad_in_portrait_is_two_panes() {
        assert_eq!(
            layout(820.0, 1180.0, &safe_area(24.0, 0.0, 20.0, 0.0)),
            PhoneLayout::TwoPane
        );
    }

    #[test]
    fn an_ipad_in_landscape_is_two_panes() {
        assert_eq!(
            layout(1180.0, 820.0, &WindowInsets::default()),
            PhoneLayout::TwoPane
        );
    }

    #[test]
    fn a_narrow_ipad_split_view_is_single() {
        assert_eq!(
            layout(375.0, 1180.0, &WindowInsets::default()),
            PhoneLayout::Single
        );
    }

    #[test]
    fn exactly_700_usable_points_wide_is_two_panes() {
        assert_eq!(
            layout(700.0, 1000.0, &WindowInsets::default()),
            PhoneLayout::TwoPane,
            "700 usable points wide fits the list beside a session"
        );
        assert_eq!(
            layout(699.9, 1000.0, &WindowInsets::default()),
            PhoneLayout::Single,
            "a tenth of a point narrower is one pane"
        );
    }

    #[test]
    fn exactly_500_usable_points_tall_is_not_terminal_full() {
        assert_eq!(
            layout(600.0, 500.0, &WindowInsets::default()),
            PhoneLayout::Single,
            "500 usable points of height keeps its header"
        );
        assert_eq!(
            layout(600.0, 499.9, &WindowInsets::default()),
            PhoneLayout::TerminalFull,
            "a tenth of a point shorter fills the screen with the terminal"
        );
    }

    #[test]
    fn the_keyboard_does_not_change_the_layout() {
        let keyboard = Edges {
            bottom: px(300.0),
            ..Edges::default()
        };
        let ipad = WindowInsets {
            ime: keyboard,
            ..safe_area(24.0, 0.0, 20.0, 0.0)
        };
        assert_eq!(
            layout(820.0, 1180.0, &ipad),
            PhoneLayout::TwoPane,
            "the keyboard never narrows the panes"
        );
        let iphone = WindowInsets {
            ime: keyboard,
            ..safe_area(62.0, 0.0, 34.0, 0.0)
        };
        assert_eq!(
            layout(402.0, 874.0, &iphone),
            PhoneLayout::Single,
            "counting the keyboard would leave 478 points of height, under the terminal's"
        );
    }
}
