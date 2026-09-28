//! The tab context menu's rows as plain data: what each row reads, the
//! selector a spec finds it by and what pressing it does.

use protocol::{MergeLayout, RearrangeLayout, TabContent, TabEntry};

use crate::layout_chooser::{best_fit_grid_cols, grid_shapes};
use crate::tabs::TabSelection;

/// What a row of the tab menu does when pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabAction {
    Rename,
    /// Swaps the rows for the Grid submenu.
    OpenGrid,
    /// Swaps the Grid submenu back for the rows.
    CloseGrid,
    Rearrange(RearrangeLayout),
    FontUp,
    FontDown,
    FontReset,
    Close,
    CloseOthers,
    Merge(MergeLayout),
}

/// A line of the tab menu.
#[derive(Debug, Clone, PartialEq, Eq)]
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

fn grid_label(cols: usize, bound: usize) -> String {
    format!(
        "{} × {}",
        counted(cols, "col"),
        counted(bound.div_ceil(cols.max(1)), "row")
    )
}

fn grid_layout(cols: usize) -> RearrangeLayout {
    RearrangeLayout::Grid {
        cols: u32::try_from(cols).unwrap_or(u32::MAX),
    }
}

/// The rearrange section for a grid of `bound` panes holding a session in
/// a pane area `aspect` wide per unit of height: nothing under two panes;
/// the section, or the Grid submenu when `grid_open`.
pub(crate) fn rearrange_lines(bound: usize, aspect: f32, grid_open: bool) -> Vec<MenuLine> {
    if bound < 2 {
        return Vec::new();
    }
    let auto = best_fit_grid_cols(bound, aspect);
    let shapes: Vec<usize> = grid_shapes(bound, aspect)
        .into_iter()
        .map(|shape| shape.cols)
        .filter(|cols| *cols > 0)
        .collect();
    if grid_open && !shapes.is_empty() {
        let mut lines = vec![
            MenuLine::row("tab-menu-grid-back", "‹ Back", TabAction::CloseGrid),
            MenuLine::row(
                "tab-menu-grid-auto",
                format!("Auto ({})", grid_label(auto, bound)),
                TabAction::Rearrange(grid_layout(auto)),
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
            "Grid (auto)",
            TabAction::Rearrange(grid_layout(auto)),
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

/// The merge section for the menu of `menu_tab`: nothing unless two tabs
/// or more are selected and `menu_tab` is one of them; disabled rows when
/// a selected tab is a diff tab, which the daemon cannot merge.
pub(crate) fn merge_lines(
    tabs: &[TabEntry],
    selection: &TabSelection,
    menu_tab: &str,
) -> Vec<MenuLine> {
    let selected = selection.in_order(tabs);
    if selected.len() < 2 || !selection.contains(menu_tab) {
        return Vec::new();
    }
    let has_diff = selected
        .iter()
        .any(|tab| matches!(tab.content, TabContent::Diff { .. }));
    let row = |selector: &str, label: &str, layout: MergeLayout| {
        if has_diff {
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
    if has_diff {
        lines.push(MenuLine::Label("Diff tabs can't be merged".to_owned()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDE: f32 = 16.0 / 10.0;

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
        assert!(rearrange_lines(0, WIDE, false).is_empty());
        assert!(rearrange_lines(1, WIDE, true).is_empty());

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
                cols: u32::try_from(best_fit_grid_cols(2, WIDE)).unwrap_or(0)
            }))
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
            MenuLine::Row { label, .. } if label == "Auto (2 cols × 2 rows)"
        ));
        assert!(matches!(
            &submenu[4],
            MenuLine::Row { label, .. } if label == "3 cols × 2 rows"
        ));
        assert_eq!(
            action(&submenu, "tab-menu-grid-3"),
            Some(TabAction::Rearrange(RearrangeLayout::Grid { cols: 3 }))
        );
    }
}
