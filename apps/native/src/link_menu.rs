//! The menu a Ctrl+Shift+click on a path link opens: what it was opened on, where
//! it sits, which row is highlighted, and the rows it offers.

use gpui::{Pixels, Point};

use crate::links::TerminalLink;
use crate::open::LinkAction;
use crate::target::HostKey;

/// An open link menu: the link it was opened on, with what a pick opens it
/// by (the folders a path resolves against, the pane's session and the host
/// its slot sends to), where it sits, and the highlighted row.
pub(crate) struct LinkMenu {
    pub(crate) link: TerminalLink,
    pub(crate) base_dirs: Vec<String>,
    pub(crate) session: Option<String>,
    pub(crate) host: HostKey,
    /// The click point, in window coordinates.
    pub(crate) at: Point<Pixels>,
    /// The highlighted row's index; Enter picks it.
    pub(crate) selected: usize,
    /// The most height its frame may take, from the window it opened in.
    pub(crate) max_height: Pixels,
}

impl LinkMenu {
    /// The menu's rows for its link.
    pub(crate) fn rows(&self) -> [LinkMenuRow; 3] {
        rows(self.link.line.is_some())
    }
}

/// One row of the link menu: its text, what picking it asks the path to do,
/// and its selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinkMenuRow {
    pub(crate) label: &'static str,
    pub(crate) action: LinkAction,
    pub(crate) id: &'static str,
}

/// The rows of the link menu, top first; `has_line` names VS Code in the
/// first, which opens a `path:line` link at its line.
pub(crate) fn rows(has_line: bool) -> [LinkMenuRow; 3] {
    [
        LinkMenuRow {
            label: if has_line {
                "Open in VS Code"
            } else {
                "Open in default app"
            },
            action: LinkAction::DefaultApp,
            id: "link-menu-default",
        },
        LinkMenuRow {
            label: "Open in viewer",
            action: LinkAction::Viewer,
            id: "link-menu-viewer",
        },
        LinkMenuRow {
            label: "Open containing folder",
            action: LinkAction::Folder,
            id: "link-menu-folder",
        },
    ]
}

/// The index of the menu's last row.
const LAST_ROW: usize = 2;

/// The row `delta` rows from `selected`, held to the menu's rows: a move
/// past either end stays on the end row.
pub(crate) fn moved(selected: usize, delta: isize) -> usize {
    selected.saturating_add_signed(delta).min(LAST_ROW)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: [&str; 3] = ["link-menu-default", "link-menu-viewer", "link-menu-folder"];

    #[test]
    fn rows_for_a_plain_path() {
        let rows = rows(false);
        assert_eq!(
            rows.map(|row| row.label),
            [
                "Open in default app",
                "Open in viewer",
                "Open containing folder"
            ]
        );
        assert_eq!(
            rows.map(|row| row.action),
            [
                LinkAction::DefaultApp,
                LinkAction::Viewer,
                LinkAction::Folder
            ]
        );
        assert_eq!(rows.map(|row| row.id), IDS);
    }

    #[test]
    fn rows_for_a_path_with_a_line() {
        let rows = rows(true);
        assert_eq!(
            rows.map(|row| row.label),
            [
                "Open in VS Code",
                "Open in viewer",
                "Open containing folder"
            ]
        );
        assert_eq!(
            rows.map(|row| row.action),
            [
                LinkAction::DefaultApp,
                LinkAction::Viewer,
                LinkAction::Folder
            ]
        );
        assert_eq!(rows.map(|row| row.id), IDS);
    }

    #[test]
    fn moving_clamps_at_both_ends() {
        assert_eq!(moved(0, -1), 0);
        assert_eq!(moved(2, 1), 2);
        assert_eq!(moved(0, 1), 1);
    }
}
