//! The phone's composer as plain Rust: which dock the composer shows above
//! its row. `session_view` draws it; the Keys dock's keys are
//! [`super::key_grid`]'s and the Quick dock's replies are
//! [`super::quick_replies`]'.

/// Which dock the composer shows above its row: one at a time, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Dock {
    /// No dock: only the composer's own row shows.
    #[default]
    None,
    /// The Quick dock's one-tap replies.
    Quick,
    /// The Keys tray.
    Keys,
}

/// One of the composer's two dock buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockButton {
    /// The Quick dock's button.
    Quick,
    /// The Keys tray's button.
    Keys,
}

impl DockButton {
    /// The dock this button opens.
    #[must_use]
    pub(crate) fn dock(self) -> Dock {
        match self {
            Self::Quick => Dock::Quick,
            Self::Keys => Dock::Keys,
        }
    }
}

impl Dock {
    /// A tap on `button`: its dock opens, or closes while it is the open
    /// one; the other dock replaces it.
    #[must_use]
    pub(crate) fn tap(self, button: DockButton) -> Self {
        if self == button.dock() {
            Self::None
        } else {
            button.dock()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tapping_quick_opens_it_and_again_closes_it() {
        assert_eq!(Dock::None.tap(DockButton::Quick), Dock::Quick);
        assert_eq!(Dock::Quick.tap(DockButton::Quick), Dock::None);
        assert_eq!(Dock::None.tap(DockButton::Keys), Dock::Keys);
        assert_eq!(Dock::Keys.tap(DockButton::Keys), Dock::None);
    }

    #[test]
    fn tapping_keys_while_quick_is_open_switches() {
        assert_eq!(Dock::Quick.tap(DockButton::Keys), Dock::Keys);
        assert_eq!(Dock::Keys.tap(DockButton::Quick), Dock::Quick);
    }
}
