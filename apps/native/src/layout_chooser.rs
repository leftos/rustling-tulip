//! The first-connect layout chooser's model: which options it shows, the
//! "Open all active sessions" picker, the keyboard's focus ring, and the
//! messages that lay the chosen arrangement out once the daemon answers.

use protocol::{ClientMessage, ClonableLayout, InitLayoutKind, RearrangeLayout};

/// The pane area's aspect the Auto grid shape assumes before the area has
/// been measured.
pub(crate) const FALLBACK_ASPECT: f32 = 16.0 / 10.0;
/// The largest "Max sessions per tab".
const MAX_PER_TAB_LIMIT: usize = 64;
/// How much of a nameless client's id its option shows.
const SHORT_ID_LEN: usize = 6;

/// How "Open all active sessions" lays the panes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Grid,
    SideBySide,
    Stacked,
}

/// A control of the chooser that takes a press and the keyboard's focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Control {
    StartEmpty,
    /// Opens or closes the arrangement picker.
    OpenAll,
    Mode(Mode),
    /// A grid shape by its column count; 0 is Auto.
    Shape(usize),
    MaxDown,
    MaxUp,
    /// Opens every active session with the picked arrangement.
    Confirm,
    AdoptLegacy,
    /// Copies the layout of the client with this id.
    Clone(String),
}

impl Control {
    /// The debug selector the control is tagged with.
    pub(crate) fn selector(&self) -> String {
        match self {
            Self::StartEmpty => "layout-choose-empty".to_owned(),
            Self::OpenAll => "layout-choose-all".to_owned(),
            Self::Mode(Mode::Grid) => "chooser-layout-grid".to_owned(),
            Self::Mode(Mode::SideBySide) => "chooser-layout-horizontal".to_owned(),
            Self::Mode(Mode::Stacked) => "chooser-layout-vertical".to_owned(),
            Self::Shape(cols) => format!("chooser-grid-shape-{cols}"),
            Self::MaxDown => "chooser-max-down".to_owned(),
            Self::MaxUp => "chooser-max-up".to_owned(),
            Self::Confirm => "chooser-confirm-all".to_owned(),
            Self::AdoptLegacy => "layout-choose-legacy".to_owned(),
            Self::Clone(client_id) => format!("layout-choose-clone-{client_id}"),
        }
    }
}

/// How the panes of every active session are laid out once the daemon has
/// put them in one tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Arrangement {
    pub(crate) layout: RearrangeLayout,
    /// At most this many panes per tab; `None` keeps them all in one.
    pub(crate) max_per_tab: Option<usize>,
}

/// What a press chose: the layout to ask the daemon for, and the
/// arrangement to apply to its answer.
pub(crate) type Choice = (InitLayoutKind, Option<Arrangement>);

/// A grid shape the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GridShape {
    /// Columns per row; 0 is Auto.
    pub(crate) cols: usize,
    pub(crate) label: String,
}

/// The open chooser.
#[derive(Debug, Clone)]
pub(crate) struct LayoutChooser {
    has_legacy: bool,
    count: usize,
    clonable: Vec<ClonableLayout>,
    expanded: bool,
    mode: Mode,
    /// The grid shape picked; 0 is Auto.
    cols: usize,
    max_per_tab: Option<usize>,
    focus: Control,
}

impl LayoutChooser {
    /// The chooser for the daemon's `LayoutInitRequired`, focused on Start
    /// empty.
    pub(crate) fn new(
        has_legacy: bool,
        active_session_count: usize,
        clonable: Vec<ClonableLayout>,
    ) -> Self {
        Self {
            has_legacy,
            count: active_session_count,
            clonable,
            expanded: false,
            mode: Mode::Grid,
            cols: 0,
            max_per_tab: None,
            focus: Control::StartEmpty,
        }
    }

    /// Every control shown, in the order Tab walks them.
    pub(crate) fn controls(&self) -> Vec<Control> {
        let mut out = vec![Control::StartEmpty];
        if self.count > 0 {
            out.push(Control::OpenAll);
            if self.expanded {
                out.push(Control::Mode(Mode::Grid));
                if self.mode == Mode::Grid {
                    out.extend(std::iter::once(0).chain(2..self.count).map(Control::Shape));
                }
                out.extend([
                    Control::Mode(Mode::SideBySide),
                    Control::Mode(Mode::Stacked),
                    Control::MaxDown,
                    Control::MaxUp,
                    Control::Confirm,
                ]);
            }
        }
        if self.has_legacy {
            out.push(Control::AdoptLegacy);
        }
        out.extend(
            self.clonable
                .iter()
                .map(|layout| Control::Clone(layout.client_id.clone())),
        );
        out
    }

    /// The control the keyboard is on: the one last focused while it is
    /// still shown, else Start empty.
    pub(crate) fn focused(&self) -> Control {
        if self.controls().contains(&self.focus) {
            self.focus.clone()
        } else {
            Control::StartEmpty
        }
    }

    /// Moves the focus to the next control shown (the previous one when
    /// not `forward`), wrapping around.
    pub(crate) fn move_focus(&mut self, forward: bool) {
        let controls = self.controls();
        let current = self.focused();
        let at = controls
            .iter()
            .position(|control| *control == current)
            .unwrap_or(0);
        let count = controls.len();
        let next = if forward {
            (at + 1) % count
        } else {
            (at + count - 1) % count
        };
        self.focus = controls[next].clone();
    }

    /// Presses `control`, which takes the focus; returns the choice when
    /// the press made one. `aspect` is the pane area's width over its
    /// height, which the Auto grid shape fits. A control not shown does
    /// nothing.
    pub(crate) fn press(&mut self, control: &Control, aspect: f32) -> Option<Choice> {
        if !self.controls().contains(control) {
            return None;
        }
        self.focus = control.clone();
        match control {
            Control::StartEmpty => return Some((InitLayoutKind::Empty, None)),
            Control::OpenAll => self.expanded = !self.expanded,
            Control::Mode(mode) => self.mode = *mode,
            Control::Shape(cols) => self.cols = *cols,
            Control::MaxDown => {
                self.max_per_tab = self
                    .max_per_tab
                    .and_then(|max| max.checked_sub(1))
                    .filter(|max| *max > 0);
            }
            Control::MaxUp => {
                self.max_per_tab = Some(
                    self.max_per_tab
                        .map_or(1, |max| (max + 1).min(MAX_PER_TAB_LIMIT)),
                );
            }
            Control::Confirm => {
                return Some((InitLayoutKind::AllSessions, Some(self.arrangement(aspect))));
            }
            Control::AdoptLegacy => return Some((InitLayoutKind::CloneLegacy, None)),
            Control::Clone(client_id) => {
                let kind = InitLayoutKind::CloneClient {
                    client_id: client_id.clone(),
                };
                return Some((kind, None));
            }
        }
        None
    }

    /// Whether the arrangement picker is open.
    pub(crate) fn expanded(&self) -> bool {
        self.expanded
    }

    /// Whether `control` shows as chosen: the open picker's toggle, the
    /// layout mode and the grid shape picked.
    pub(crate) fn is_selected(&self, control: &Control) -> bool {
        match control {
            Control::OpenAll => self.expanded,
            Control::Mode(mode) => self.mode == *mode,
            Control::Shape(cols) => self.cols == *cols,
            _ => false,
        }
    }

    /// What `control` reads; `aspect` resolves the Auto shape's size.
    pub(crate) fn label(&self, control: &Control, aspect: f32) -> String {
        match control {
            Control::StartEmpty => "Start empty".to_owned(),
            Control::OpenAll => "Open all active sessions".to_owned(),
            Control::Mode(Mode::Grid) => "Grid".to_owned(),
            Control::Mode(Mode::SideBySide) => "Side by side".to_owned(),
            Control::Mode(Mode::Stacked) => "Stacked".to_owned(),
            Control::Shape(cols) => grid_shapes(self.count, aspect)
                .into_iter()
                .find(|shape| shape.cols == *cols)
                .map(|shape| shape.label)
                .unwrap_or_default(),
            Control::MaxDown => "−".to_owned(),
            Control::MaxUp => "+".to_owned(),
            Control::Confirm => self.confirm_label(),
            Control::AdoptLegacy => "Adopt the previous layout".to_owned(),
            Control::Clone(client_id) => self.clone_label(client_id),
        }
    }

    /// The muted line under an option, where it has one.
    pub(crate) fn detail(&self, control: &Control) -> Option<String> {
        let detail = match control {
            Control::StartEmpty => "add the sessions you want yourself".to_owned(),
            Control::OpenAll => format!("one pane per running session ({})", self.count),
            Control::AdoptLegacy => "the tabs from before per-window layouts".to_owned(),
            Control::Clone(_) => "same sessions, your own independent panes".to_owned(),
            _ => return None,
        };
        Some(detail)
    }

    /// The max-per-tab stepper's value: `all`, or the limit.
    pub(crate) fn max_label(&self) -> String {
        self.max_per_tab
            .map_or_else(|| "all".to_owned(), |max| max.to_string())
    }

    fn confirm_label(&self) -> String {
        let sessions = format!("Open {} {}", self.count, plural(self.count, "session"));
        let Some(max) = self.max_per_tab else {
            return sessions;
        };
        let tabs = self.count.div_ceil(max);
        format!("{sessions}, {tabs} {}", plural(tabs, "tab"))
    }

    fn clone_label(&self, client_id: &str) -> String {
        let name = self
            .clonable
            .iter()
            .find(|layout| layout.client_id == client_id)
            .and_then(|layout| layout.name.clone());
        if let Some(name) = name {
            format!("Copy {name}'s layout")
        } else {
            let short: String = client_id.chars().take(SHORT_ID_LEN).collect();
            format!("Copy another window's layout ({short})")
        }
    }

    fn arrangement(&self, aspect: f32) -> Arrangement {
        let layout = match self.mode {
            Mode::SideBySide => RearrangeLayout::Horizontal,
            Mode::Stacked => RearrangeLayout::Vertical,
            Mode::Grid => {
                let cols = if self.cols == 0 {
                    best_fit_grid_cols(self.count, aspect)
                } else {
                    self.cols
                };
                RearrangeLayout::Grid {
                    cols: u32::try_from(cols).unwrap_or(u32::MAX),
                }
            }
        };
        Arrangement {
            layout,
            max_per_tab: self.max_per_tab,
        }
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
    }
}

/// The grid shapes for `n` panes: Auto (fitted to `aspect`), then every
/// column count strictly between one column and `n`, each `cols × rows`.
pub(crate) fn grid_shapes(n: usize, aspect: f32) -> Vec<GridShape> {
    let auto = best_fit_grid_cols(n, aspect);
    let auto_shape = GridShape {
        cols: 0,
        label: format!("Auto ({auto} × {})", n.div_ceil(auto)),
    };
    std::iter::once(auto_shape)
        .chain((2..n).map(|cols| GridShape {
            cols,
            label: format!("{cols} × {}", n.div_ceil(cols)),
        }))
        .collect()
}

/// The column count whose cells come closest to square for `n` panes in an
/// area `aspect` wide per unit of height, with each empty cell weighing
/// 0.3 and a one-row or one-column strip of five or more 0.4 more; an
/// aspect that is not a positive number counts as 16:9. Ported from the
/// Tauri client's `bestFitGridCols`.
pub(crate) fn best_fit_grid_cols(n: usize, aspect: f32) -> usize {
    if n <= 1 {
        return 1;
    }
    let aspect = f64::from(aspect);
    let aspect = if aspect > 0.0 && aspect.is_finite() {
        aspect
    } else {
        16.0 / 9.0
    };
    let mut best = 1;
    let mut best_score = f64::INFINITY;
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let empties = rows * cols - n;
        let cell_aspect = aspect * as_f64(rows) / as_f64(cols);
        let mut score = cell_aspect.ln().abs() + 0.3 * as_f64(empties);
        if (cols == 1 || cols == n) && n >= 5 {
            score += 0.4;
        }
        if score < best_score {
            best_score = score;
            best = cols;
        }
    }
    best
}

fn as_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// What lays `pane_ids`, the first tab's panes in order, out as
/// `arrangement`: every batch of `max_per_tab` after the first moves to a
/// tab of its own, then the first tab is rearranged.
pub(crate) fn arrangement_messages(
    tab_id: &str,
    pane_ids: &[String],
    arrangement: Arrangement,
) -> Vec<ClientMessage> {
    let per_tab = arrangement.max_per_tab.unwrap_or(pane_ids.len()).max(1);
    let mut out: Vec<ClientMessage> = pane_ids
        .get(per_tab..)
        .unwrap_or_default()
        .chunks(per_tab)
        .map(|batch| ClientMessage::ExtractToNewTab {
            source_tab_id: tab_id.to_owned(),
            pane_ids: batch.to_vec(),
            name: None,
            layout: Some(arrangement.layout),
        })
        .collect();
    out.push(ClientMessage::RearrangeTab {
        tab_id: tab_id.to_owned(),
        layout: arrangement.layout,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASPECT: f32 = 16.0 / 10.0;

    fn clonable(id: &str, name: Option<&str>) -> ClonableLayout {
        ClonableLayout {
            client_id: id.to_owned(),
            name: name.map(str::to_owned),
        }
    }

    fn chooser(count: usize) -> LayoutChooser {
        LayoutChooser::new(false, count, Vec::new())
    }

    fn confirm_label(model: &LayoutChooser) -> String {
        model.label(&Control::Confirm, ASPECT)
    }

    #[test]
    fn start_empty_is_focused_and_sends_empty() {
        let mut model = chooser(3);
        assert_eq!(model.focused(), Control::StartEmpty);
        let choice = model.press(&Control::StartEmpty, ASPECT);
        assert!(
            matches!(choice, Some((InitLayoutKind::Empty, None))),
            "{choice:?}"
        );
    }

    #[test]
    fn open_all_hidden_when_count_is_zero() {
        assert!(!chooser(0).controls().contains(&Control::OpenAll));
        assert!(chooser(1).controls().contains(&Control::OpenAll));
        let mut empty = chooser(0);
        assert!(
            empty.press(&Control::Confirm, ASPECT).is_none(),
            "a control not shown does nothing"
        );
    }

    #[test]
    fn adopt_previous_only_with_legacy() {
        assert!(!chooser(1).controls().contains(&Control::AdoptLegacy));
        let mut legacy = LayoutChooser::new(true, 1, Vec::new());
        assert!(legacy.controls().contains(&Control::AdoptLegacy));
        assert_eq!(
            legacy.label(&Control::AdoptLegacy, ASPECT),
            "Adopt the previous layout"
        );
        let choice = legacy.press(&Control::AdoptLegacy, ASPECT);
        assert!(
            matches!(choice, Some((InitLayoutKind::CloneLegacy, None))),
            "{choice:?}"
        );
    }

    #[test]
    fn nameless_clonable_shows_short_id() {
        let mut model = LayoutChooser::new(
            false,
            0,
            vec![
                clonable("abcdef123456", None),
                clonable("99", Some("DESKTOP-1")),
            ],
        );
        let nameless = Control::Clone("abcdef123456".to_owned());
        assert_eq!(
            model.label(&nameless, ASPECT),
            "Copy another window's layout (abcdef)"
        );
        assert_eq!(
            model.label(&Control::Clone("99".to_owned()), ASPECT),
            "Copy DESKTOP-1's layout"
        );
        let choice = model.press(&nameless, ASPECT);
        assert!(
            matches!(
                &choice,
                Some((InitLayoutKind::CloneClient { client_id }, None)) if client_id == "abcdef123456"
            ),
            "{choice:?}"
        );
    }

    #[test]
    fn grid_shapes_list_auto_then_k_by_ceil() {
        // 5 panes at 16:10: Auto resolves to 3 columns (see the cases below).
        let shapes = grid_shapes(5, ASPECT);
        let labels: Vec<(usize, &str)> = shapes
            .iter()
            .map(|shape| (shape.cols, shape.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            vec![
                (0, "Auto (3 × 2)"),
                (2, "2 × 3"),
                (3, "3 × 2"),
                (4, "4 × 2")
            ]
        );
        assert_eq!(
            grid_shapes(2, 1.0).len(),
            1,
            "no shape between 1 and N columns"
        );

        let mut model = chooser(5);
        assert!(!model.controls().contains(&Control::Shape(0)), "collapsed");
        model.press(&Control::OpenAll, ASPECT);
        assert!(
            model.controls().contains(&Control::Shape(4)),
            "grid is the default"
        );
        assert_eq!(model.label(&Control::Shape(0), ASPECT), "Auto (3 × 2)");
        model.press(&Control::Mode(Mode::SideBySide), ASPECT);
        assert!(
            !model.controls().contains(&Control::Shape(0)),
            "shapes are grid-only"
        );
    }

    /// Worked by hand from `utils/grid.ts`'s score
    /// `|ln(aspect × rows / cols)| + 0.3 × empties (+ 0.4 for a strip at 5+)`:
    /// - 4 at 16:9: 2 cols scores 0.575, 3 cols 0.770, 4 cols 0.811 → 2.
    /// - 9 at 16:9: 3 cols 0.575, 5 cols 0.641, 4 cols 1.188 → 3.
    /// - 5 at 16:10: 3 cols 0.365, 4 cols 1.123, 2 cols 1.175 → 3.
    /// - 3 at 1:2: 1 col 0.405, 2 cols 0.993, 3 cols 1.792 → 1.
    /// - an aspect that is not positive falls back to 16:9, so 4 → 2.
    #[test]
    fn best_fit_grid_cols_matches_tauri_cases() {
        assert_eq!(best_fit_grid_cols(1, 16.0 / 9.0), 1);
        assert_eq!(best_fit_grid_cols(4, 16.0 / 9.0), 2);
        assert_eq!(best_fit_grid_cols(9, 16.0 / 9.0), 3);
        assert_eq!(best_fit_grid_cols(5, 16.0 / 10.0), 3);
        assert_eq!(best_fit_grid_cols(3, 0.5), 1);
        assert_eq!(best_fit_grid_cols(4, 0.0), 2);
    }

    #[test]
    fn max_stepper_goes_all_then_1_to_64() {
        let mut model = chooser(5);
        model.press(&Control::OpenAll, ASPECT);
        assert_eq!(model.max_label(), "all");
        model.press(&Control::MaxDown, ASPECT);
        assert_eq!(model.max_label(), "all", "all is the floor");
        model.press(&Control::MaxUp, ASPECT);
        assert_eq!(model.max_label(), "1");
        assert_eq!(confirm_label(&model), "Open 5 sessions, 5 tabs");
        model.press(&Control::MaxUp, ASPECT);
        assert_eq!(confirm_label(&model), "Open 5 sessions, 3 tabs");
        for _ in 0..100 {
            model.press(&Control::MaxUp, ASPECT);
        }
        assert_eq!(model.max_label(), "64", "64 is the ceiling");
        assert_eq!(confirm_label(&model), "Open 5 sessions, 1 tab");
        for _ in 0..63 {
            model.press(&Control::MaxDown, ASPECT);
        }
        assert_eq!(model.max_label(), "1");
        model.press(&Control::MaxDown, ASPECT);
        assert_eq!(model.max_label(), "all");
        assert_eq!(confirm_label(&model), "Open 5 sessions");
        assert_eq!(confirm_label(&chooser(1)), "Open 1 session");
    }

    #[test]
    fn confirm_resolves_the_layout() {
        let mut model = chooser(5);
        model.press(&Control::OpenAll, ASPECT);
        let auto = model.press(&Control::Confirm, ASPECT);
        let expected = Arrangement {
            layout: RearrangeLayout::Grid { cols: 3 },
            max_per_tab: None,
        };
        assert!(
            matches!(&auto, Some((InitLayoutKind::AllSessions, Some(a))) if *a == expected),
            "{auto:?}"
        );
        model.press(&Control::Shape(2), ASPECT);
        model.press(&Control::MaxUp, ASPECT);
        let chosen = model.press(&Control::Confirm, ASPECT);
        let expected = Arrangement {
            layout: RearrangeLayout::Grid { cols: 2 },
            max_per_tab: Some(1),
        };
        assert!(
            matches!(&chosen, Some((InitLayoutKind::AllSessions, Some(a))) if *a == expected),
            "{chosen:?}"
        );
        for (mode, layout) in [
            (Mode::Stacked, RearrangeLayout::Vertical),
            (Mode::SideBySide, RearrangeLayout::Horizontal),
        ] {
            model.press(&Control::Mode(mode), ASPECT);
            let choice = model.press(&Control::Confirm, ASPECT);
            assert!(
                matches!(&choice, Some((_, Some(a))) if a.layout == layout),
                "{choice:?}"
            );
        }
    }

    #[test]
    fn batches_split_after_the_first_by_max() {
        let panes: Vec<String> = (1..=5).map(|n| format!("p{n}")).collect();
        let layout = RearrangeLayout::Grid { cols: 2 };
        let limited = Arrangement {
            layout,
            max_per_tab: Some(2),
        };
        let sent = arrangement_messages("t1", &panes, limited);
        let extracted: Vec<Vec<String>> = sent
            .iter()
            .filter_map(|m| match m {
                ClientMessage::ExtractToNewTab {
                    source_tab_id,
                    pane_ids,
                    name: None,
                    layout: Some(l),
                } if source_tab_id == "t1" && *l == layout => Some(pane_ids.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            extracted,
            vec![
                vec!["p3".to_owned(), "p4".to_owned()],
                vec!["p5".to_owned()]
            ]
        );
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert!(
            matches!(
                sent.last(),
                Some(ClientMessage::RearrangeTab { tab_id, layout: l }) if tab_id == "t1" && *l == layout
            ),
            "{sent:?}"
        );

        for max_per_tab in [None, Some(5), Some(64)] {
            let sent = arrangement_messages(
                "t1",
                &panes,
                Arrangement {
                    layout: RearrangeLayout::Horizontal,
                    max_per_tab,
                },
            );
            assert!(
                matches!(sent.as_slice(), [ClientMessage::RearrangeTab { .. }]),
                "{max_per_tab:?}: {sent:?}"
            );
        }
        let lone = arrangement_messages(
            "t1",
            &[],
            Arrangement {
                layout,
                max_per_tab: Some(2),
            },
        );
        assert!(
            matches!(lone.as_slice(), [ClientMessage::RearrangeTab { .. }]),
            "{lone:?}"
        );
    }

    #[test]
    fn tab_cycles_visible_controls_and_wraps() {
        let mut model = LayoutChooser::new(true, 3, vec![clonable("c1", None)]);
        let collapsed = vec![
            Control::StartEmpty,
            Control::OpenAll,
            Control::AdoptLegacy,
            Control::Clone("c1".to_owned()),
        ];
        assert_eq!(model.controls(), collapsed);
        let mut seen = Vec::new();
        for _ in 0..collapsed.len() {
            model.move_focus(true);
            seen.push(model.focused());
        }
        assert_eq!(
            seen,
            vec![
                Control::OpenAll,
                Control::AdoptLegacy,
                Control::Clone("c1".to_owned()),
                Control::StartEmpty,
            ],
            "wraps forward"
        );
        model.move_focus(false);
        assert_eq!(
            model.focused(),
            Control::Clone("c1".to_owned()),
            "wraps back"
        );

        assert!(model.press(&Control::OpenAll, ASPECT).is_none());
        assert_eq!(
            model.controls(),
            vec![
                Control::StartEmpty,
                Control::OpenAll,
                Control::Mode(Mode::Grid),
                Control::Shape(0),
                Control::Shape(2),
                Control::Mode(Mode::SideBySide),
                Control::Mode(Mode::Stacked),
                Control::MaxDown,
                Control::MaxUp,
                Control::Confirm,
                Control::AdoptLegacy,
                Control::Clone("c1".to_owned()),
            ]
        );
        assert_eq!(model.focused(), Control::OpenAll, "a press takes the focus");
        model.move_focus(true);
        assert_eq!(model.focused(), Control::Mode(Mode::Grid));
        model.press(&Control::Mode(Mode::Stacked), ASPECT);
        model.move_focus(false);
        assert_eq!(
            model.focused(),
            Control::Mode(Mode::SideBySide),
            "the shapes left the ring with grid mode"
        );
    }
}
