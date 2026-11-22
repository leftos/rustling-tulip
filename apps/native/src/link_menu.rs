//! The menu a Ctrl+Shift+click on a path link opens, and the pick menu a
//! plain Ctrl+click opens when the same path sits in several of the
//! session's folders: what it was opened on, where it sits, which row is
//! highlighted, and the rows it offers.

use gpui::{Pixels, Point, SharedString};

use crate::links::TerminalLink;
use crate::open::{LinkAction, OpenAction};
use crate::open_view::action_path;
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
    /// The files to pick among, one row each, when a plain click found the
    /// path in several session folders; empty for the action menu, whose
    /// rows are its link's actions.
    pub(crate) choices: Vec<OpenAction>,
    /// What the click asked a file to do: a picked choice opens that way.
    pub(crate) action: LinkAction,
    /// The click point, in window coordinates.
    pub(crate) at: Point<Pixels>,
    /// The highlighted row's index; Enter picks it.
    pub(crate) selected: usize,
    /// The most height its frame may take, from the window it opened in.
    pub(crate) max_height: Pixels,
}

impl LinkMenu {
    /// The menu's rows: one per choice when it picks among files, else its
    /// link's actions.
    pub(crate) fn rows(&self) -> Vec<LinkMenuRow> {
        if self.choices.is_empty() {
            rows(self.link.line.is_some(), self.follow)
        } else {
            pick_rows(&self.choices)
        }
    }
}

/// What picking a link menu row does: open the path as a [`LinkAction`]
/// says, follow it in a follow tab, or open the choice at that index of the
/// menu's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowPick {
    Open(LinkAction),
    Follow,
    Choice(usize),
}

/// One row of the link menu: its text, what picking it does, and its
/// selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkMenuRow {
    pub(crate) label: SharedString,
    pub(crate) pick: RowPick,
    pub(crate) id: SharedString,
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
            }
            .into(),
            pick: RowPick::Open(LinkAction::DefaultApp),
            id: "link-menu-default".into(),
        },
        LinkMenuRow {
            label: "Open in viewer".into(),
            pick: RowPick::Open(LinkAction::Viewer),
            id: "link-menu-viewer".into(),
        },
    ];
    if follow {
        rows.push(LinkMenuRow {
            label: "Follow in tab".into(),
            pick: RowPick::Follow,
            id: "link-menu-follow".into(),
        });
    }
    rows.push(LinkMenuRow {
        label: "Open containing folder".into(),
        pick: RowPick::Open(LinkAction::Folder),
        id: "link-menu-folder".into(),
    });
    rows
}

/// The rows of a menu that picks among files: one per choice, top first,
/// each labelled with the file it opens as the choice holds it and tagged
/// `link-menu-pick-<index>`.
pub(crate) fn pick_rows(choices: &[OpenAction]) -> Vec<LinkMenuRow> {
    choices
        .iter()
        .enumerate()
        .map(|(index, action)| LinkMenuRow {
            label: action_path(action).display().to_string().into(),
            pick: RowPick::Choice(index),
            id: format!("link-menu-pick-{index}").into(),
        })
        .collect()
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
    use std::path::PathBuf;

    use super::*;

    const IDS: [&str; 3] = ["link-menu-default", "link-menu-viewer", "link-menu-folder"];

    fn labels(rows: &[LinkMenuRow]) -> Vec<&str> {
        rows.iter().map(|row| row.label.as_ref()).collect()
    }

    fn picks(rows: &[LinkMenuRow]) -> Vec<RowPick> {
        rows.iter().map(|row| row.pick).collect()
    }

    fn ids(rows: &[LinkMenuRow]) -> Vec<&str> {
        rows.iter().map(|row| row.id.as_ref()).collect()
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
    fn pick_rows_list_each_choice_in_order() {
        let choices = [
            OpenAction::DefaultApp(PathBuf::from(r"C:/a/src/lib.rs")),
            OpenAction::DefaultApp(PathBuf::from(r"C:/b/src/lib.rs")),
        ];
        let rows = pick_rows(&choices);
        assert_eq!(
            rows.iter().map(|row| row.id.as_ref()).collect::<Vec<_>>(),
            ["link-menu-pick-0", "link-menu-pick-1"]
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_ref())
                .collect::<Vec<_>>(),
            [r"C:/a/src/lib.rs", r"C:/b/src/lib.rs"]
        );
        assert_eq!(
            rows.iter().map(|row| row.pick).collect::<Vec<_>>(),
            [RowPick::Choice(0), RowPick::Choice(1)]
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
