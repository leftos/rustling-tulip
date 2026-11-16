//! The phone's composer as plain Rust: the Keys tray's bytes and which dock
//! the composer shows above its row. `session_view` draws it; the Quick
//! dock's replies are [`super::quick_replies`].

/// One button of the Keys tray: the name on it and what a tap types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyEntry {
    /// The name drawn on the button.
    pub(crate) label: &'static str,
    /// The bytes a tap types.
    pub(crate) bytes: &'static [u8],
}

/// The Keys tray's strip, in collie's order: Esc, Tab, Shift+Tab, the two
/// arrows, Enter and Ctrl+C, then the digits 1 to 9 as themselves.
pub(crate) const KEY_TRAY: &[KeyEntry] = &[
    KeyEntry {
        label: "Esc",
        bytes: b"\x1b",
    },
    KeyEntry {
        label: "Tab",
        bytes: b"\t",
    },
    KeyEntry {
        label: "Shift+Tab",
        bytes: b"\x1b[Z",
    },
    KeyEntry {
        label: "↑",
        bytes: b"\x1b[A",
    },
    KeyEntry {
        label: "↓",
        bytes: b"\x1b[B",
    },
    KeyEntry {
        label: "Enter",
        bytes: b"\r",
    },
    KeyEntry {
        label: "Ctrl+C",
        bytes: b"\x03",
    },
    KeyEntry {
        label: "1",
        bytes: b"1",
    },
    KeyEntry {
        label: "2",
        bytes: b"2",
    },
    KeyEntry {
        label: "3",
        bytes: b"3",
    },
    KeyEntry {
        label: "4",
        bytes: b"4",
    },
    KeyEntry {
        label: "5",
        bytes: b"5",
    },
    KeyEntry {
        label: "6",
        bytes: b"6",
    },
    KeyEntry {
        label: "7",
        bytes: b"7",
    },
    KeyEntry {
        label: "8",
        bytes: b"8",
    },
    KeyEntry {
        label: "9",
        bytes: b"9",
    },
];

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
    fn the_key_tray_bytes_are_the_pwas() {
        let keys: Vec<(&str, &[u8])> = KEY_TRAY.iter().map(|key| (key.label, key.bytes)).collect();
        assert_eq!(
            keys,
            vec![
                ("Esc", &b"\x1b"[..]),
                ("Tab", &b"\t"[..]),
                ("Shift+Tab", &b"\x1b[Z"[..]),
                ("↑", &b"\x1b[A"[..]),
                ("↓", &b"\x1b[B"[..]),
                ("Enter", &b"\r"[..]),
                ("Ctrl+C", &b"\x03"[..]),
                ("1", &b"1"[..]),
                ("2", &b"2"[..]),
                ("3", &b"3"[..]),
                ("4", &b"4"[..]),
                ("5", &b"5"[..]),
                ("6", &b"6"[..]),
                ("7", &b"7"[..]),
                ("8", &b"8"[..]),
                ("9", &b"9"[..]),
            ]
        );
    }

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
