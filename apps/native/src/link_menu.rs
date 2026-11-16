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
    /// Whether `base_dirs` are a dispatch run's own folders, which a
    /// remote host's path is searched in instead of the session's.
    pub(crate) run_folders: bool,
    pub(crate) host: HostKey,
    /// Whether the menu offers "Follow in tab": the pane has a session, on
    /// a host whose daemon follows files.
    pub(crate) follow: bool,
    /// The click point, in window coordinates.
    pub(crate) at: Point<Pixels>,
    /// The highlighted row's index; Enter picks it.
    pub(crate) selected: usize,
    /// The most height its frame may take, from the window it opened in.
    pub(crate) max_height: Pixels,
}

impl LinkMenu {
    /// The menu's rows for its link.
    pub(crate) fn rows(&self) -> Vec<LinkMenuRow> {
        rows(self.link.line.is_some(), self.follow)
    }
}

/// What picking a link menu row does: open the path as a [`LinkAction`]
/// says, or follow it in a follow tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowPick {
    Open(LinkAction),
    Follow,
}

/// One row of the link menu: its text, what picking it does, and its
/// selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinkMenuRow {
    pub(crate) label: &'static str,
    pub(crate) pick: RowPick,
    pub(crate) id: &'static str,
}

/// The rows of the link menu, top first; `has_line` names VS Code in the
/// first, which opens a `path:line` link at its line, and `follow` adds
/// "Follow in tab" before the folder row.
pub(crate) fn rows(has_line: bool, follow: bool) -> Vec<LinkMenuRow> {
    let mut rows = vec![
        LinkMenuRow {
            label: if has_line {
                "Open in VS Code"
            } else {
                "Open in default app"
            },
            pick: RowPick::Open(LinkAction::DefaultApp),
            id: "link-menu-default",
        },
        LinkMenuRow {
            label: "Open in viewer",
            pick: RowPick::Open(LinkAction::Viewer),
            id: "link-menu-viewer",
        },
    ];
    if follow {
        rows.push(LinkMenuRow {
            label: "Follow in tab",
            pick: RowPick::Follow,
            id: "link-menu-follow",
        });
    }
    rows.push(LinkMenuRow {
        label: "Open containing folder",
        pick: RowPick::Open(LinkAction::Folder),
        id: "link-menu-folder",
    });
    rows
}

/// The row `delta` rows from `selected` in a menu of `count` rows, held to
/// them: a move past either end stays on the end row.
pub(crate) fn moved(selected: usize, delta: isize, count: usize) -> usize {
    selected
        .saturating_add_signed(delta)
        .min(count.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: [&str; 3] = ["link-menu-default", "link-menu-viewer", "link-menu-folder"];

    fn labels(rows: &[LinkMenuRow]) -> Vec<&'static str> {
        rows.iter().map(|row| row.label).collect()
    }

    fn picks(rows: &[LinkMenuRow]) -> Vec<RowPick> {
        rows.iter().map(|row| row.pick).collect()
    }

    fn ids(rows: &[LinkMenuRow]) -> Vec<&'static str> {
        rows.iter().map(|row| row.id).collect()
    }

    #[test]
    fn rows_for_a_plain_path() {
        let rows = rows(false, false);
        assert_eq!(
            labels(&rows),
            [
                "Open in default app",
                "Open in viewer",
                "Open containing folder"
            ]
        );
        assert_eq!(
            picks(&rows),
            [
                RowPick::Open(LinkAction::DefaultApp),
                RowPick::Open(LinkAction::Viewer),
                RowPick::Open(LinkAction::Folder)
            ]
        );
        assert_eq!(ids(&rows), IDS);
    }

    #[test]
    fn rows_for_a_path_with_a_line() {
        let rows = rows(true, false);
        assert_eq!(
            labels(&rows),
            [
                "Open in VS Code",
                "Open in viewer",
                "Open containing folder"
            ]
        );
        assert_eq!(
            picks(&rows),
            [
                RowPick::Open(LinkAction::DefaultApp),
                RowPick::Open(LinkAction::Viewer),
                RowPick::Open(LinkAction::Folder)
            ]
        );
        assert_eq!(ids(&rows), IDS);
    }

    #[test]
    fn rows_with_the_follow_row() {
        let rows = rows(false, true);
        assert_eq!(
            labels(&rows),
            [
                "Open in default app",
                "Open in viewer",
                "Follow in tab",
                "Open containing folder"
            ]
        );
        assert_eq!(
            picks(&rows),
            [
                RowPick::Open(LinkAction::DefaultApp),
                RowPick::Open(LinkAction::Viewer),
                RowPick::Follow,
                RowPick::Open(LinkAction::Folder)
            ]
        );
        assert_eq!(
            ids(&rows),
            [
                "link-menu-default",
                "link-menu-viewer",
                "link-menu-follow",
                "link-menu-folder"
            ]
        );
    }

    #[test]
    fn moving_clamps_at_both_ends_of_three_rows() {
        assert_eq!(moved(0, -1, 3), 0);
        assert_eq!(moved(2, 1, 3), 2);
        assert_eq!(moved(0, 1, 3), 1);
    }

    #[test]
    fn moving_clamps_at_both_ends_of_four_rows() {
        assert_eq!(moved(0, -1, 4), 0);
        assert_eq!(moved(2, 1, 4), 3);
        assert_eq!(moved(3, 1, 4), 3);
        assert_eq!(moved(3, -1, 4), 2);
    }
}
