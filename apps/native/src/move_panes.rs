//! The Move panes dialog as plain data: the panes on offer and which are
//! ticked, the layout and grid shape, its controls in the keyboard's order,
//! and the extract it sends.

use protocol::{ClientMessage, RearrangeLayout};

use crate::layout_chooser::{best_fit_grid_cols, grid_shapes};
use crate::tab_menu::grid_label;

pub(crate) const TITLE: &str = "Move panes to a new tab";
pub(crate) const NAME_LABEL: &str = "Name (optional)";
pub(crate) const NAME_PLACEHOLDER: &str = "New tab name";

/// How the new tab arranges the moved panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutChoice {
    SideBySide,
    Stacked,
    Grid,
}

impl LayoutChoice {
    pub(crate) const ALL: [Self; 3] = [Self::SideBySide, Self::Stacked, Self::Grid];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::SideBySide => "Side by side",
            Self::Stacked => "Stacked",
            Self::Grid => "Grid",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::SideBySide => "side-by-side",
            Self::Stacked => "stacked",
            Self::Grid => "grid",
        }
    }
}

/// A control of the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Control {
    /// The checkbox of the pane at this index.
    Pane(usize),
    Layout(LayoutChoice),
    /// A grid shape by its columns; 0 is Auto.
    Shape(usize),
    Name,
    Cancel,
    Confirm,
    /// The ✕ in the title row.
    Dismiss,
}

/// The open Move panes dialog.
#[derive(Debug, Clone)]
pub(crate) struct MovePanes {
    tab_id: String,
    tab_name: String,
    /// Each bound pane in grid order: its id and its session's label.
    panes: Vec<(String, String)>,
    ticked: Vec<bool>,
    layout: LayoutChoice,
    /// The grid shape's columns; 0 is Auto.
    shape: usize,
    focus: Control,
}

impl MovePanes {
    /// The dialog for `panes` of `tab_id`, none ticked, side by side, the
    /// first checkbox focused.
    pub(crate) fn new(tab_id: &str, tab_name: &str, panes: Vec<(String, String)>) -> Self {
        Self {
            tab_id: tab_id.to_owned(),
            tab_name: tab_name.to_owned(),
            ticked: vec![false; panes.len()],
            panes,
            layout: LayoutChoice::SideBySide,
            shape: 0,
            focus: Control::Pane(0),
        }
    }

    pub(crate) fn tab_id(&self) -> &str {
        &self.tab_id
    }

    pub(crate) fn hint(&self) -> String {
        format!(
            "Pick the panes to move out of \"{}\". They're removed from this tab and arranged in a new one with the layout below.",
            self.tab_name
        )
    }

    pub(crate) fn panes(&self) -> &[(String, String)] {
        &self.panes
    }

    pub(crate) fn is_ticked(&self, index: usize) -> bool {
        self.ticked.get(index).copied().unwrap_or(false)
    }

    fn ticked_count(&self) -> usize {
        self.ticked.iter().filter(|t| **t).count()
    }

    pub(crate) fn confirm_enabled(&self) -> bool {
        self.ticked_count() > 0
    }

    pub(crate) fn confirm_label(&self) -> String {
        match self.ticked_count() {
            0 => "Move panes".to_owned(),
            1 => "Move 1 pane".to_owned(),
            n => format!("Move {n} panes"),
        }
    }

    /// Ticks or unticks the pane at `index`. A grid shape the new count
    /// no longer offers falls back to Auto.
    pub(crate) fn toggle(&mut self, index: usize) {
        if let Some(ticked) = self.ticked.get_mut(index) {
            *ticked = !*ticked;
        }
        if self.shape != 0 && self.shape >= self.ticked_count() {
            self.shape = 0;
        }
    }

    /// Drops the panes `keep` rejects, with their ticks, and returns how
    /// many remain. A focus or a grid shape that no longer exists falls
    /// back to the first checkbox and Auto.
    pub(crate) fn retain_panes(&mut self, keep: impl Fn(&str) -> bool) -> usize {
        let (panes, ticked): (Vec<_>, Vec<_>) = std::mem::take(&mut self.panes)
            .into_iter()
            .zip(std::mem::take(&mut self.ticked))
            .filter(|((id, _), _)| keep(id))
            .unzip();
        self.panes = panes;
        self.ticked = ticked;
        if matches!(self.focus, Control::Pane(index) if index >= self.panes.len()) {
            self.focus = Control::Pane(0);
        }
        if self.shape != 0 && self.shape >= self.ticked_count() {
            self.shape = 0;
        }
        self.panes.len()
    }

    pub(crate) fn layout(&self) -> LayoutChoice {
        self.layout
    }

    pub(crate) fn set_layout(&mut self, layout: LayoutChoice) {
        self.layout = layout;
    }

    pub(crate) fn shape(&self) -> usize {
        self.shape
    }

    pub(crate) fn set_shape(&mut self, cols: usize) {
        self.shape = cols;
    }

    /// The grid shapes for the ticked count, as `(cols, label)`: Auto
    /// (with what it resolves to once a pane is ticked), then each fixed
    /// shape.
    pub(crate) fn shapes(&self, aspect: f32) -> Vec<(usize, String)> {
        let n = self.ticked_count();
        if n == 0 {
            return vec![(0, "Auto".to_owned())];
        }
        let auto = best_fit_grid_cols(n, aspect);
        let mut shapes = vec![(0, format!("Auto ({})", grid_label(auto, n)))];
        shapes.extend(
            grid_shapes(n, aspect)
                .into_iter()
                .filter(|shape| shape.cols > 0)
                .map(|shape| (shape.cols, grid_label(shape.cols, n))),
        );
        shapes
    }

    /// Every control in the keyboard's order: the checkboxes, the layout
    /// choices, the shapes while Grid is chosen, the name, Cancel, Move
    /// and ✕.
    pub(crate) fn controls(&self, aspect: f32) -> Vec<Control> {
        let mut controls: Vec<Control> = (0..self.panes.len()).map(Control::Pane).collect();
        controls.extend(LayoutChoice::ALL.map(Control::Layout));
        if self.layout == LayoutChoice::Grid {
            controls.extend(
                self.shapes(aspect)
                    .into_iter()
                    .map(|(cols, _)| Control::Shape(cols)),
            );
        }
        controls.extend([
            Control::Name,
            Control::Cancel,
            Control::Confirm,
            Control::Dismiss,
        ]);
        controls
    }

    pub(crate) fn focused(&self) -> Control {
        self.focus
    }

    pub(crate) fn set_focus(&mut self, control: Control) {
        self.focus = control;
    }

    /// Moves the focus to the next control, or the previous one when not
    /// `forward`, wrapping at either end.
    pub(crate) fn move_focus(&mut self, forward: bool, aspect: f32) {
        let controls = self.controls(aspect);
        let len = controls.len();
        let at = controls.iter().position(|c| *c == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        self.focus = controls[next];
    }

    /// The selector a spec finds `control` by.
    pub(crate) fn selector(&self, control: Control) -> String {
        match control {
            Control::Pane(index) => {
                let id = self.panes.get(index).map_or("", |(id, _)| id.as_str());
                format!("move-panes-pane-{id}")
            }
            Control::Layout(layout) => format!("move-panes-layout-{}", layout.slug()),
            Control::Shape(0) => "move-panes-shape-auto".to_owned(),
            Control::Shape(cols) => format!("move-panes-shape-{cols}"),
            Control::Name => "move-panes-name".to_owned(),
            Control::Cancel => "move-panes-cancel".to_owned(),
            Control::Confirm => "move-panes-confirm".to_owned(),
            Control::Dismiss => "move-panes-close".to_owned(),
        }
    }

    /// The extract the confirm sends, `name` trimmed and a blank one left
    /// out, Auto resolved for a pane area `aspect` wide per unit of
    /// height; `None` with nothing ticked.
    pub(crate) fn message(&self, name: &str, aspect: f32) -> Option<ClientMessage> {
        let pane_ids: Vec<String> = self
            .panes
            .iter()
            .zip(&self.ticked)
            .filter(|(_, ticked)| **ticked)
            .map(|((id, _), _)| id.clone())
            .collect();
        if pane_ids.is_empty() {
            return None;
        }
        let layout = match self.layout {
            LayoutChoice::SideBySide => RearrangeLayout::Horizontal,
            LayoutChoice::Stacked => RearrangeLayout::Vertical,
            LayoutChoice::Grid => {
                let cols = match self.shape {
                    0 => best_fit_grid_cols(pane_ids.len(), aspect),
                    cols => cols,
                };
                RearrangeLayout::Grid {
                    cols: u32::try_from(cols).unwrap_or(u32::MAX),
                }
            }
        };
        let name = name.trim();
        Some(ClientMessage::ExtractToNewTab {
            source_tab_id: self.tab_id.clone(),
            pane_ids,
            name: (!name.is_empty()).then(|| name.to_owned()),
            layout: Some(layout),
        })
    }
}

#[cfg(test)]
mod tests {
    use protocol::{ClientMessage, RearrangeLayout};

    use super::*;
    use crate::layout_chooser::best_fit_grid_cols;

    const WIDE: f32 = 16.0 / 10.0;

    fn dialog(n: usize) -> MovePanes {
        let panes = (1..=n)
            .map(|i| (format!("p{i}"), format!("s{i}")))
            .collect();
        MovePanes::new("t1", "Main", panes)
    }

    #[test]
    fn move_panes_confirm_disabled_at_zero_and_label_pluralises() {
        let mut d = dialog(4);
        assert_eq!(
            d.hint(),
            "Pick the panes to move out of \"Main\". They're removed from this tab and arranged in a new one with the layout below."
        );
        assert_eq!(d.focused(), Control::Pane(0), "the first box has the focus");
        assert!(!d.confirm_enabled());
        assert_eq!(d.confirm_label(), "Move panes");
        assert!(d.message("", WIDE).is_none());
        d.toggle(1);
        assert!(d.confirm_enabled());
        assert_eq!(d.confirm_label(), "Move 1 pane");
        d.toggle(3);
        assert_eq!(d.confirm_label(), "Move 2 panes");
        d.toggle(1);
        d.toggle(3);
        assert!(!d.confirm_enabled(), "unticking all disables it again");
    }

    #[test]
    fn shape_falls_back_to_auto_when_selection_shrinks() {
        let mut d = dialog(5);
        d.set_layout(LayoutChoice::Grid);
        assert_eq!(
            d.shapes(WIDE),
            [(0, "Auto".to_owned())],
            "nothing ticked: Auto alone"
        );
        for i in 0..5 {
            d.toggle(i);
        }
        let auto = best_fit_grid_cols(5, WIDE);
        let shapes = d.shapes(WIDE);
        assert_eq!(
            shapes[0],
            (
                0,
                format!("Auto ({auto} cols × {} rows)", 5_usize.div_ceil(auto))
            )
        );
        assert_eq!(
            shapes[1..],
            [
                (2, "2 cols × 3 rows".to_owned()),
                (3, "3 cols × 2 rows".to_owned()),
                (4, "4 cols × 2 rows".to_owned())
            ]
        );
        d.set_shape(4);
        assert_eq!(d.shape(), 4);
        d.toggle(4);
        assert_eq!(d.shape(), 0, "4 of 4 has no 4-col shape: Auto");
        d.set_shape(3);
        d.toggle(3);
        assert_eq!(d.shape(), 0, "3 of 3 has no 3-col shape: Auto");
        d.set_shape(2);
        d.toggle(3);
        assert_eq!(d.shape(), 2, "a growing selection keeps the shape");
    }

    #[test]
    fn move_panes_message_carries_ticked_panes_in_grid_order() {
        let mut d = dialog(4);
        d.toggle(3);
        d.toggle(0);
        d.toggle(2);
        let Some(ClientMessage::ExtractToNewTab {
            source_tab_id,
            pane_ids,
            name,
            layout,
        }) = d.message("  ", WIDE)
        else {
            unreachable!("no extract");
        };
        assert_eq!(source_tab_id, "t1");
        assert_eq!(pane_ids, ["p1", "p3", "p4"]);
        assert_eq!(name, None, "a blank name is none");
        assert_eq!(layout, Some(RearrangeLayout::Horizontal));

        d.set_layout(LayoutChoice::Stacked);
        let msg = d.message(" Work ", WIDE);
        assert!(
            matches!(
                &msg,
                Some(ClientMessage::ExtractToNewTab { name: Some(name), layout: Some(RearrangeLayout::Vertical), .. })
                    if name == "Work"
            ),
            "{msg:?}"
        );

        d.set_layout(LayoutChoice::Grid);
        let cols = |msg: Option<ClientMessage>| match msg {
            Some(ClientMessage::ExtractToNewTab {
                layout: Some(RearrangeLayout::Grid { cols }),
                ..
            }) => Some(cols),
            _ => None,
        };
        let auto = u32::try_from(best_fit_grid_cols(3, WIDE)).unwrap_or(0);
        assert_eq!(cols(d.message("", WIDE)), Some(auto), "Auto fits the area");
        assert_eq!(cols(d.message("", 0.2)), Some(1), "a tall area stacks them");
        d.set_shape(2);
        assert_eq!(cols(d.message("", WIDE)), Some(2));
    }

    #[test]
    fn move_panes_focus_ring_includes_shapes_only_with_grid() {
        let mut d = dialog(3);
        d.toggle(0);
        d.toggle(1);
        d.toggle(2);
        let ring = d.controls(WIDE);
        assert_eq!(
            ring,
            [
                Control::Pane(0),
                Control::Pane(1),
                Control::Pane(2),
                Control::Layout(LayoutChoice::SideBySide),
                Control::Layout(LayoutChoice::Stacked),
                Control::Layout(LayoutChoice::Grid),
                Control::Name,
                Control::Cancel,
                Control::Confirm,
                Control::Dismiss,
            ]
        );
        d.set_layout(LayoutChoice::Grid);
        let ring = d.controls(WIDE);
        assert_eq!(ring[6..8], [Control::Shape(0), Control::Shape(2)]);
        d.move_focus(false, WIDE);
        assert_eq!(d.focused(), Control::Dismiss, "Shift+Tab wraps");
    }
}
