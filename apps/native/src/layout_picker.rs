//! The layout picker shared by the first-connect chooser and the host rebind
//! dialog: a layout mode, a grid shape and a maximum of panes per tab, and the
//! arrangement they make.

use protocol::RearrangeLayout;

/// The pane area's aspect the Auto grid shape assumes before the area has
/// been measured.
pub(crate) const FALLBACK_ASPECT: f32 = 16.0 / 10.0;
/// The largest "Max sessions per tab".
pub(crate) const MAX_PER_TAB_LIMIT: usize = 64;

/// How the picked panes are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Grid,
    SideBySide,
    Stacked,
}

/// How the panes are laid out once they share a tab.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Arrangement {
    pub(crate) layout: RearrangeLayout,
    /// At most this many panes per tab; `None` keeps them all in one.
    pub(crate) max_per_tab: Option<usize>,
}

/// A grid shape the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GridShape {
    /// Columns per row; 0 is Auto.
    pub(crate) cols: usize,
    pub(crate) label: String,
}

/// The picker's state for `count` panes.
#[derive(Debug, Clone)]
pub(crate) struct LayoutPicker {
    count: usize,
    mode: Mode,
    /// The grid shape picked; 0 is Auto.
    cols: usize,
    max_per_tab: Option<usize>,
}

impl LayoutPicker {
    /// The picker for `count` panes on the Auto grid, with `max_per_tab`.
    pub(crate) fn new(count: usize, max_per_tab: Option<usize>) -> Self {
        Self {
            count,
            mode: Mode::Grid,
            cols: 0,
            max_per_tab,
        }
    }

    /// How many panes the picker lays out.
    pub(crate) fn count(&self) -> usize {
        self.count
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// The grid shape picked; 0 is Auto.
    pub(crate) fn cols(&self) -> usize {
        self.cols
    }

    pub(crate) fn max_per_tab(&self) -> Option<usize> {
        self.max_per_tab
    }

    pub(crate) fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Picks the grid shape of `cols` columns; 0 is Auto.
    pub(crate) fn set_shape(&mut self, cols: usize) {
        self.cols = cols;
    }

    /// One fewer per tab; below 1 is all in one tab, which is the floor.
    pub(crate) fn max_down(&mut self) {
        self.max_per_tab = self
            .max_per_tab
            .and_then(|max| max.checked_sub(1))
            .filter(|max| *max > 0);
    }

    /// One more per tab; all in one tab steps to 1, and
    /// [`MAX_PER_TAB_LIMIT`] is the ceiling.
    pub(crate) fn max_up(&mut self) {
        self.max_per_tab = Some(
            self.max_per_tab
                .map_or(1, |max| (max + 1).min(MAX_PER_TAB_LIMIT)),
        );
    }

    /// The column counts of the grid shapes offered: Auto (0), then every
    /// count strictly between one column and the pane count.
    pub(crate) fn shape_cols(&self) -> impl Iterator<Item = usize> {
        std::iter::once(0).chain(2..self.count)
    }

    /// What the shape of `cols` columns reads; empty when it is not offered.
    pub(crate) fn shape_label(&self, cols: usize) -> String {
        grid_shapes(self.count)
            .into_iter()
            .find(|shape| shape.cols == cols)
            .map(|shape| shape.label)
            .unwrap_or_default()
    }

    /// The max-per-tab stepper's value: `all`, or the limit.
    pub(crate) fn max_label(&self) -> String {
        self.max_per_tab
            .map_or_else(|| "all".to_owned(), |max| max.to_string())
    }

    /// The arrangement picked; `aspect` is the pane area's width over its
    /// height, which the Auto grid carries.
    pub(crate) fn arrangement(&self, aspect: f32) -> Arrangement {
        let layout = match self.mode {
            Mode::SideBySide => RearrangeLayout::Horizontal,
            Mode::Stacked => RearrangeLayout::Vertical,
            Mode::Grid => {
                // Auto names no count and carries the area's shape, so the
                // daemon re-picks the columns when the area changes; a picked
                // shape keeps the fixed count it names.
                let auto = self.cols == 0;
                RearrangeLayout::Grid {
                    cols: if auto {
                        0
                    } else {
                        u32::try_from(self.cols).unwrap_or(u32::MAX)
                    },
                    aspect: auto.then_some(aspect),
                }
            }
        };
        Arrangement {
            layout,
            max_per_tab: self.max_per_tab,
        }
    }
}

/// The grid shapes for `n` panes: Auto, then every column count strictly
/// between one column and `n`, each `cols × rows`. Auto's count belongs to the
/// daemon, which fits it to the pane area.
pub(crate) fn grid_shapes(n: usize) -> Vec<GridShape> {
    let auto_shape = GridShape {
        cols: 0,
        label: "Auto".to_owned(),
    };
    std::iter::once(auto_shape)
        .chain((2..n).map(|cols| GridShape {
            cols,
            label: format!("{cols} × {}", n.div_ceil(cols)),
        }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASPECT: f32 = 16.0 / 10.0;

    #[test]
    fn starts_on_the_auto_grid_with_the_given_max() {
        let picker = LayoutPicker::new(5, Some(4));
        assert_eq!(picker.mode(), Mode::Grid);
        assert_eq!(picker.cols(), 0);
        assert_eq!(picker.max_label(), "4");
        assert_eq!(
            picker.arrangement(ASPECT),
            Arrangement {
                layout: RearrangeLayout::Grid {
                    cols: 0,
                    aspect: Some(ASPECT),
                },
                max_per_tab: Some(4),
            }
        );
    }

    #[test]
    fn max_steps_between_all_and_the_limit() {
        let mut picker = LayoutPicker::new(5, None);
        picker.max_down();
        assert_eq!(picker.max_per_tab(), None, "all is the floor");
        picker.max_up();
        assert_eq!(picker.max_per_tab(), Some(1));
        for _ in 0..100 {
            picker.max_up();
        }
        assert_eq!(picker.max_per_tab(), Some(MAX_PER_TAB_LIMIT));
        picker.max_down();
        assert_eq!(picker.max_per_tab(), Some(MAX_PER_TAB_LIMIT - 1));
    }

    #[test]
    fn shapes_and_modes_pick_the_layout() {
        let mut picker = LayoutPicker::new(5, None);
        assert_eq!(picker.shape_cols().collect::<Vec<_>>(), vec![0, 2, 3, 4]);
        assert_eq!(picker.shape_label(3), "3 × 2");
        assert_eq!(picker.shape_label(9), "", "not offered");
        picker.set_shape(2);
        assert_eq!(
            picker.arrangement(ASPECT).layout,
            RearrangeLayout::Grid {
                cols: 2,
                aspect: None,
            }
        );
        picker.set_mode(Mode::SideBySide);
        assert_eq!(
            picker.arrangement(ASPECT).layout,
            RearrangeLayout::Horizontal
        );
        picker.set_mode(Mode::Stacked);
        assert_eq!(picker.arrangement(ASPECT).layout, RearrangeLayout::Vertical);
    }
}
