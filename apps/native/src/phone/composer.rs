//! The phone's composer as plain Rust: the Quick dock's one-tap replies,
//! the Keys tray's bytes, and which dock the composer shows above its row.
//! `session_view` draws it.

/// One section of the Quick dock: its label and the replies it offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuickGroup {
    /// The section label drawn above the replies.
    pub(crate) title: &'static str,
    /// The texts sent, each typed into the session and submitted.
    pub(crate) items: &'static [&'static str],
}

/// The replies an agent session offers. Every harness reads "yes" and
/// "continue" the same way.
const AGENT_GROUPS: &[QuickGroup] = &[
    QuickGroup {
        title: "confirm",
        items: &["yes", "no"],
    },
    QuickGroup {
        title: "common",
        items: &[
            "continue",
            "commit and push",
            "retry",
            "skip",
            "drastically simplify",
        ],
    },
];

/// The replies a session with no agent offers: a bare shell has no turn to
/// continue or skip, only the classic y/n pair.
const SHELL_GROUPS: &[QuickGroup] = &[QuickGroup {
    title: "confirm",
    items: &["y", "n"],
}];

/// The Quick dock's groups for a session with an agent or without one.
#[must_use]
pub(crate) fn quick_groups(has_agent: bool) -> &'static [QuickGroup] {
    if has_agent {
        AGENT_GROUPS
    } else {
        SHELL_GROUPS
    }
}

/// The bytes a quick reply types: its text, then `\r`, which submits it.
#[must_use]
pub(crate) fn quick_bytes(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(b'\r');
    bytes
}

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
    fn an_agent_session_gets_the_agent_quick_groups() {
        let groups = quick_groups(true);
        let titles: Vec<&str> = groups.iter().map(|group| group.title).collect();
        assert_eq!(titles, ["confirm", "common"]);
        assert_eq!(groups[0].items, ["yes", "no"]);
        assert_eq!(
            groups[1].items,
            [
                "continue",
                "commit and push",
                "retry",
                "skip",
                "drastically simplify"
            ]
        );
    }

    #[test]
    fn a_shell_session_gets_only_y_and_n() {
        let groups = quick_groups(false);
        assert_eq!(groups.len(), 1, "a bare shell has no common group");
        assert_eq!(groups[0].title, "confirm");
        assert_eq!(groups[0].items, ["y", "n"]);
    }

    #[test]
    fn a_quick_reply_types_its_text_then_return() {
        assert_eq!(quick_bytes("continue"), b"continue\r");
        assert_eq!(quick_bytes("yes"), b"yes\r");
    }

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
