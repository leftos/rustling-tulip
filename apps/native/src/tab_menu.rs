//! The tab context menu's rows as plain data: what each row reads, the
//! selector a spec finds it by and what pressing it does.

use protocol::{MergeLayout, RearrangeLayout, TabContent, TabEntry};

use crate::layout_chooser::grid_shapes;
use crate::popouts::{POP_OUT_ALREADY_TIP, PopoutKey, Popouts};
use crate::tabs::TabSelection;

/// Why the tab menu's Pop out is dimmed for a viewer tab.
pub(crate) const VIEWER_POP_OUT_TIP: &str = "Viewer tabs can't be popped out";
/// Why the tab menu's Pop out is dimmed for a diff tab.
pub(crate) const DIFF_POP_OUT_TIP: &str = "Diff tabs can't be popped out";
/// The tab menu's Pop out row, which reads Focus window once the tab is out.
const POP_OUT_SELECTOR: &str = "tab-menu-popout";

/// What a row of the tab menu does when pressed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TabAction {
    Rename,
    /// Opens the tab in a window of its own, or raises its open pop-out.
    PopOut,
    /// Swaps the rows for the Grid submenu.
    OpenGrid,
    /// Swaps the Grid submenu back for the rows.
    CloseGrid,
    Rearrange(RearrangeLayout),
    /// Opens the Move panes dialog.
    MovePanes,
    FontUp,
    FontDown,
    FontReset,
    Close,
    CloseOthers,
    Merge(MergeLayout),
}

/// A line of the tab menu.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MenuLine {
    /// A muted line that does nothing.
    Label(String),
    Separator,
    /// A row; with no action it is shown disabled.
    Row {
        selector: String,
        label: String,
        action: Option<TabAction>,
        danger: bool,
    },
}

impl MenuLine {
    pub(crate) fn row(selector: &str, label: impl Into<String>, action: TabAction) -> Self {
        Self::Row {
            selector: selector.to_owned(),
            label: label.into(),
            action: Some(action),
            danger: false,
        }
    }

    pub(crate) fn inert(selector: &str, label: impl Into<String>) -> Self {
        Self::Row {
            selector: selector.to_owned(),
            label: label.into(),
            action: None,
            danger: false,
        }
    }

    /// The row's selector; `None` for a label or a separator.
    pub(crate) fn selector(&self) -> Option<&str> {
        match self {
            Self::Row { selector, .. } => Some(selector),
            Self::Label(_) | Self::Separator => None,
        }
    }
}

/// `count` of `noun`, with an `s` unless it is one.
fn counted(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// `<c> cols × <r> rows` for `bound` panes in `cols` columns.
pub(crate) fn grid_label(cols: usize, bound: usize) -> String {
    format!(
        "{} × {}",
        counted(cols, "col"),
        counted(bound.div_ceil(cols.max(1)), "row")
    )
}

fn grid_layout(cols: usize) -> RearrangeLayout {
    RearrangeLayout::Grid {
        cols: u32::try_from(cols).unwrap_or(u32::MAX),
        aspect: None,
    }
}

/// The automatic grid: no column count, and the pane area's shape so the
/// daemon fits the columns now and re-picks them when the area changes.
fn auto_grid_layout(aspect: f32) -> RearrangeLayout {
    RearrangeLayout::Grid {
        cols: 0,
        aspect: Some(aspect),
    }
}

/// The rearrange section for a grid of `bound` panes in a pane area `aspect`
/// wide per unit of height: nothing under two panes; the section, or the Grid
/// submenu when `grid_open`.
pub(crate) fn rearrange_lines(bound: usize, aspect: f32, grid_open: bool) -> Vec<MenuLine> {
    if bound < 2 {
        return Vec::new();
    }
    let shapes: Vec<usize> = grid_shapes(bound)
        .into_iter()
        .map(|shape| shape.cols)
        .filter(|cols| *cols > 0)
        .collect();
    if grid_open && !shapes.is_empty() {
        let mut lines = vec![
            MenuLine::row("tab-menu-grid-back", "‹ Back", TabAction::CloseGrid),
            MenuLine::row(
                "tab-menu-grid-auto",
                "Auto",
                TabAction::Rearrange(auto_grid_layout(aspect)),
            ),
            MenuLine::Separator,
        ];
        lines.extend(shapes.into_iter().map(|cols| {
            MenuLine::row(
                &format!("tab-menu-grid-{cols}"),
                grid_label(cols, bound),
                TabAction::Rearrange(grid_layout(cols)),
            )
        }));
        return lines;
    }
    let grid = if shapes.is_empty() {
        MenuLine::row(
            "tab-menu-grid-auto",
            "Auto",
            TabAction::Rearrange(auto_grid_layout(aspect)),
        )
    } else {
        MenuLine::row("tab-menu-grid", "Grid ▸", TabAction::OpenGrid)
    };
    vec![
        MenuLine::Label("Rearrange panes".to_owned()),
        grid,
        MenuLine::row(
            "tab-menu-side-by-side",
            "Side by side",
            TabAction::Rearrange(RearrangeLayout::Horizontal),
        ),
        MenuLine::row(
            "tab-menu-stacked",
            "Stacked",
            TabAction::Rearrange(RearrangeLayout::Vertical),
        ),
    ]
}

/// The first lines of `tab`'s menu: Rename (not for a viewer tab, whose
/// pill shows its file's name), then Pop out.
pub(crate) fn head_lines(tab: &TabEntry, tabs: &[TabEntry], popouts: &Popouts) -> Vec<MenuLine> {
    let mut lines = Vec::new();
    if !matches!(tab.content, TabContent::Viewer { .. }) {
        lines.push(MenuLine::row(
            "tab-menu-rename",
            "Rename tab",
            TabAction::Rename,
        ));
    }
    lines.extend(pop_out_lines(tab, tabs, popouts));
    lines
}

/// `tab`'s Pop out: Focus window while the tab has its own window, else
/// Pop out, dimmed under a line naming why for a viewer or diff tab and for
/// a grid tab an open pop-out already shares content with.
fn pop_out_lines(tab: &TabEntry, tabs: &[TabEntry], popouts: &Popouts) -> Vec<MenuLine> {
    let refusal = match &tab.content {
        TabContent::Viewer { .. } => VIEWER_POP_OUT_TIP,
        TabContent::Diff { .. } => DIFF_POP_OUT_TIP,
        TabContent::Grid { .. } => {
            let key = PopoutKey::Tab {
                tab_id: tab.id.clone(),
            };
            match popouts.blocking(&key, tabs) {
                None => {
                    return vec![MenuLine::row(
                        POP_OUT_SELECTOR,
                        "Pop out",
                        TabAction::PopOut,
                    )];
                }
                Some(open) if open == key => {
                    return vec![MenuLine::row(
                        POP_OUT_SELECTOR,
                        "Focus window",
                        TabAction::PopOut,
                    )];
                }
                Some(_) => POP_OUT_ALREADY_TIP,
            }
        }
    };
    vec![
        MenuLine::inert(POP_OUT_SELECTOR, "Pop out"),
        MenuLine::Label(refusal.to_owned()),
    ]
}

/// The merge section for the menu of `menu_tab`: nothing unless two tabs
/// or more are selected and `menu_tab` is one of them; disabled rows when
/// a selected tab has no grid (a diff or viewer tab), which the daemon
/// cannot merge, under a line naming the first such tab's kind.
pub(crate) fn merge_lines(
    tabs: &[TabEntry],
    selection: &TabSelection,
    menu_tab: &str,
) -> Vec<MenuLine> {
    let selected = selection.in_order(tabs);
    if selected.len() < 2 || !selection.contains(menu_tab) {
        return Vec::new();
    }
    let unmergeable = selected.iter().find_map(|tab| match tab.content {
        TabContent::Diff { .. } => Some("Diff tabs can't be merged"),
        TabContent::Viewer { .. } => Some("Viewer tabs can't be merged"),
        TabContent::Grid { .. } => None,
    });
    let row = |selector: &str, label: &str, layout: MergeLayout| {
        if unmergeable.is_some() {
            MenuLine::inert(selector, label)
        } else {
            MenuLine::row(selector, label, TabAction::Merge(layout))
        }
    };
    let mut lines = vec![
        MenuLine::Label(format!("Merge {} selected into new tab", selected.len())),
        row(
            "tab-menu-merge-horizontal",
            "Side by side (horizontal)",
            MergeLayout::TileHorizontal,
        ),
        row(
            "tab-menu-merge-vertical",
            "Stacked (vertical)",
            MergeLayout::TileVertical,
        ),
    ];
    if let Some(note) = unmergeable {
        lines.push(MenuLine::Label(note.to_owned()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::popouts::{PopoutKey, Popouts};
    use crate::tabs::tests::{diff_tab, pane, split, tab};
    use protocol::SplitDirection;

    const WIDE: f32 = 16.0 / 10.0;

    /// `t1` holding `p1` on `s1` and `p2` on `s2`.
    fn grid_tab() -> TabEntry {
        tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                0.5,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )
    }

    /// The label and action of `selector`'s row.
    fn row_of<'a>(lines: &'a [MenuLine], selector: &str) -> Option<(&'a str, Option<TabAction>)> {
        lines.iter().find_map(|line| match line {
            MenuLine::Row {
                selector: s,
                label,
                action,
                ..
            } if s == selector => Some((label.as_str(), *action)),
            _ => None,
        })
    }

    fn labels(lines: &[MenuLine]) -> Vec<&str> {
        lines
            .iter()
            .filter_map(|line| match line {
                MenuLine::Label(text) => Some(text.as_str()),
                MenuLine::Row { .. } | MenuLine::Separator => None,
            })
            .collect()
    }

    #[test]
    fn pop_out_row_follows_rename() {
        let t1 = grid_tab();
        let lines = head_lines(&t1, std::slice::from_ref(&t1), &Popouts::default());

        assert_eq!(selectors(&lines), ["tab-menu-rename", "tab-menu-popout"]);
        assert_eq!(
            row_of(&lines, "tab-menu-popout"),
            Some(("Pop out", Some(TabAction::PopOut)))
        );
        assert_eq!(
            labels(&lines),
            [] as [&str; 0],
            "an enabled row gives no reason"
        );
    }

    #[test]
    fn pop_out_row_is_disabled_for_a_viewer_tab() {
        let mut viewer = grid_tab();
        viewer.content = TabContent::Viewer {
            path: "C:\\notes\\a.md".to_owned(),
        };
        let lines = head_lines(&viewer, std::slice::from_ref(&viewer), &Popouts::default());

        assert_eq!(
            selectors(&lines),
            ["tab-menu-popout"],
            "a viewer tab has no Rename"
        );
        assert_eq!(row_of(&lines, "tab-menu-popout"), Some(("Pop out", None)));
        assert_eq!(labels(&lines), ["Viewer tabs can't be popped out"]);
    }

    #[test]
    fn pop_out_row_is_disabled_for_a_diff_tab() {
        let diff = diff_tab("d1");
        let lines = head_lines(&diff, std::slice::from_ref(&diff), &Popouts::default());

        assert_eq!(selectors(&lines), ["tab-menu-rename", "tab-menu-popout"]);
        assert_eq!(row_of(&lines, "tab-menu-popout"), Some(("Pop out", None)));
        assert_eq!(labels(&lines), ["Diff tabs can't be popped out"]);
    }

    #[test]
    fn pop_out_row_is_disabled_when_a_pane_is_popped() {
        let tabs = vec![grid_tab(), tab("t2", &pane("p3", Some("s3")))];
        let mut popouts = Popouts::default();
        popouts.open(
            PopoutKey::Pane {
                tab_id: "t1".to_owned(),
                pane_id: "p2".to_owned(),
            },
            &tabs,
        );

        let lines = head_lines(&tabs[0], &tabs, &popouts);
        assert_eq!(row_of(&lines, "tab-menu-popout"), Some(("Pop out", None)));
        assert_eq!(labels(&lines), ["Already in a pop-out"]);

        let mut sessions = Popouts::default();
        sessions.open(
            PopoutKey::Session {
                session_id: "s1".to_owned(),
            },
            &tabs,
        );
        let lines = head_lines(&tabs[0], &tabs, &sessions);
        assert_eq!(
            labels(&lines),
            ["Already in a pop-out"],
            "a pane's session in a session pop-out overlaps too"
        );
        let lines = head_lines(&tabs[1], &tabs, &popouts);
        assert_eq!(
            row_of(&lines, "tab-menu-popout"),
            Some(("Pop out", Some(TabAction::PopOut))),
            "another tab is not held back"
        );
    }

    #[test]
    fn pop_out_row_reads_focus_window_for_a_popped_tab() {
        let tabs = vec![grid_tab()];
        let mut popouts = Popouts::default();
        popouts.open(
            PopoutKey::Tab {
                tab_id: "t1".to_owned(),
            },
            &tabs,
        );

        let lines = head_lines(&tabs[0], &tabs, &popouts);
        assert_eq!(
            row_of(&lines, "tab-menu-popout"),
            Some(("Focus window", Some(TabAction::PopOut)))
        );
        assert_eq!(labels(&lines), [] as [&str; 0]);
    }

    fn selectors(lines: &[MenuLine]) -> Vec<&str> {
        lines.iter().filter_map(MenuLine::selector).collect()
    }

    fn action(lines: &[MenuLine], selector: &str) -> Option<TabAction> {
        lines.iter().find_map(|line| match line {
            MenuLine::Row {
                selector: s,
                action,
                ..
            } if s == selector => *action,
            _ => None,
        })
    }

    #[test]
    fn rearrange_rows_follow_bound_count() {
        assert_eq!(
            rearrange_lines(0, WIDE, false),
            [] as [crate::tab_menu::MenuLine; 0]
        );
        assert_eq!(
            rearrange_lines(1, WIDE, true),
            [] as [crate::tab_menu::MenuLine; 0]
        );

        let two = rearrange_lines(2, WIDE, false);
        assert_eq!(two[0], MenuLine::Label("Rearrange panes".to_owned()));
        assert_eq!(
            selectors(&two),
            [
                "tab-menu-grid-auto",
                "tab-menu-side-by-side",
                "tab-menu-stacked"
            ],
            "two panes have no shape beyond Auto, so Grid is a flat row"
        );
        assert_eq!(
            action(&two, "tab-menu-grid-auto"),
            Some(TabAction::Rearrange(RearrangeLayout::Grid {
                cols: 0,
                aspect: Some(WIDE)
            })),
            "Auto asks the daemon to fit the pane area"
        );
        assert_eq!(
            action(&two, "tab-menu-stacked"),
            Some(TabAction::Rearrange(RearrangeLayout::Vertical))
        );

        let four = rearrange_lines(4, WIDE, false);
        assert_eq!(
            selectors(&four),
            ["tab-menu-grid", "tab-menu-side-by-side", "tab-menu-stacked"]
        );
        assert_eq!(action(&four, "tab-menu-grid"), Some(TabAction::OpenGrid));

        let submenu = rearrange_lines(4, WIDE, true);
        assert_eq!(
            selectors(&submenu),
            [
                "tab-menu-grid-back",
                "tab-menu-grid-auto",
                "tab-menu-grid-2",
                "tab-menu-grid-3"
            ]
        );
        assert!(matches!(
            &submenu[1],
            MenuLine::Row { label, .. } if label == "Auto"
        ));
        assert!(matches!(
            &submenu[4],
            MenuLine::Row { label, .. } if label == "3 cols × 2 rows"
        ));
        assert_eq!(
            action(&submenu, "tab-menu-grid-3"),
            Some(TabAction::Rearrange(RearrangeLayout::Grid {
                cols: 3,
                aspect: None
            }))
        );
    }
}
