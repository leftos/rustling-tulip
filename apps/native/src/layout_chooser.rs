//! The first-connect layout chooser's model: which options it shows, the
//! "Open all active sessions" picker (a [`LayoutPicker`]), the keyboard's
//! focus ring, and the messages that lay the chosen arrangement out once the
//! daemon answers.

use protocol::{ClientMessage, ClonableLayout, InitLayoutKind};

pub(crate) use crate::layout_picker::Arrangement;
use crate::layout_picker::{LayoutPicker, Mode};

/// How much of a nameless client's id its option shows.
const SHORT_ID_LEN: usize = 6;

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

/// What a press chose: the layout to ask the daemon for, and the
/// arrangement to apply to its answer.
pub(crate) type Choice = (InitLayoutKind, Option<Arrangement>);

/// The open chooser.
#[derive(Debug, Clone)]
pub(crate) struct LayoutChooser {
    /// Whether the question came from another host, whose tab layout this
    /// window does not draw.
    remote: bool,
    has_legacy: bool,
    clonable: Vec<ClonableLayout>,
    expanded: bool,
    /// The "Open all" picker, over the active sessions' count.
    picker: LayoutPicker,
    focus: Control,
}

impl LayoutChooser {
    /// The chooser for a host's `LayoutInitRequired`, focused on Start
    /// empty. `remote` is whether that host is another machine: its layout is
    /// not this window's to draw, so it offers nothing to choose.
    pub(crate) fn new(
        remote: bool,
        has_legacy: bool,
        active_session_count: usize,
        clonable: Vec<ClonableLayout>,
    ) -> Self {
        Self {
            remote,
            has_legacy,
            clonable,
            expanded: false,
            picker: LayoutPicker::new(active_session_count, None),
            focus: Control::StartEmpty,
        }
    }

    /// Every control shown, in the order Tab walks them; a remote host
    /// offers Start empty alone, since the tab it would open is not one this
    /// window draws.
    pub(crate) fn controls(&self) -> Vec<Control> {
        let mut out = vec![Control::StartEmpty];
        if self.remote {
            return out;
        }
        if self.picker.count() > 0 {
            out.push(Control::OpenAll);
            if self.expanded {
                out.push(Control::Mode(Mode::Grid));
                if self.picker.mode() == Mode::Grid {
                    out.extend(self.picker.shape_cols().map(Control::Shape));
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

    /// Whether Start empty is the only control the chooser offers, so asking
    /// would offer no choice.
    pub(crate) fn only_start_empty(&self) -> bool {
        self.controls() == [Control::StartEmpty]
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
        if let Some(next) = step_focus(&self.controls(), &self.focused(), forward) {
            self.focus = next;
        }
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
            Control::Mode(mode) => self.picker.set_mode(*mode),
            Control::Shape(cols) => self.picker.set_shape(*cols),
            Control::MaxDown => self.picker.max_down(),
            Control::MaxUp => self.picker.max_up(),
            Control::Confirm => {
                let arrangement = self.picker.arrangement(aspect);
                return Some((InitLayoutKind::AllSessions, Some(arrangement)));
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
            Control::Mode(mode) => self.picker.mode() == *mode,
            Control::Shape(cols) => self.picker.cols() == *cols,
            _ => false,
        }
    }

    /// What `control` reads; the pane area's aspect no longer resolves a
    /// count here, since the daemon fits the Auto grid.
    pub(crate) fn label(&self, control: &Control, _aspect: f32) -> String {
        match control {
            Control::StartEmpty => "Start empty".to_owned(),
            Control::OpenAll => "Open all active sessions".to_owned(),
            Control::Mode(Mode::Grid) => "Grid".to_owned(),
            Control::Mode(Mode::SideBySide) => "Side by side".to_owned(),
            Control::Mode(Mode::Stacked) => "Stacked".to_owned(),
            Control::Shape(cols) => self.picker.shape_label(*cols),
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
            Control::OpenAll => {
                format!("one pane per running session ({})", self.picker.count())
            }
            Control::AdoptLegacy => "the tabs from before per-window layouts".to_owned(),
            Control::Clone(_) => "same sessions, your own independent panes".to_owned(),
            _ => return None,
        };
        Some(detail)
    }

    /// The max-per-tab stepper's value: `all`, or the limit.
    pub(crate) fn max_label(&self) -> String {
        self.picker.max_label()
    }

    fn confirm_label(&self) -> String {
        let count = self.picker.count();
        let sessions = format!("Open {count} {}", plural(count, "session"));
        let Some(max) = self.picker.max_per_tab() else {
            return sessions;
        };
        let tabs = count.div_ceil(max);
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
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
    }
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
            request_id: None,
        })
        .collect();
    out.push(ClientMessage::RearrangeTab {
        tab_id: tab_id.to_owned(),
        layout: arrangement.layout,
    });
    out
}

/// The control after `current` in `controls` (before it when not
/// `forward`), wrapping around; the first when `current` is not there.
fn step_focus<T: PartialEq + Clone>(controls: &[T], current: &T, forward: bool) -> Option<T> {
    let count = controls.len();
    if count == 0 {
        return None;
    }
    let at = controls
        .iter()
        .position(|control| control == current)
        .unwrap_or(0);
    let next = if forward {
        (at + 1) % count
    } else {
        (at + count - 1) % count
    };
    controls.get(next).cloned()
}

#[cfg(test)]
mod tests {
    use protocol::RearrangeLayout;

    use super::*;
    use crate::layout_picker::grid_shapes;

    const ASPECT: f32 = 16.0 / 10.0;

    fn clonable(id: &str, name: Option<&str>) -> ClonableLayout {
        ClonableLayout {
            client_id: id.to_owned(),
            name: name.map(str::to_owned),
        }
    }

    fn chooser(count: usize) -> LayoutChooser {
        LayoutChooser::new(false, false, count, Vec::new())
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
    fn only_start_empty_when_nothing_else_is_offered() {
        assert!(chooser(0).only_start_empty(), "no options");
        assert!(!chooser(1).only_start_empty(), "one active session");
        assert!(
            !LayoutChooser::new(false, true, 0, Vec::new()).only_start_empty(),
            "a legacy layout"
        );
        assert!(
            !LayoutChooser::new(false, false, 0, vec![clonable("abc", None)]).only_start_empty(),
            "a clonable layout"
        );
    }

    #[test]
    fn a_remote_hosts_chooser_offers_only_start_empty() {
        let remote = LayoutChooser::new(true, true, 3, vec![clonable("c1", None)]);
        assert_eq!(
            remote.controls(),
            [Control::StartEmpty],
            "no Open all, no picker, no Adopt legacy and no Clone"
        );
        assert!(
            remote.only_start_empty(),
            "a remote host's question is answered without asking"
        );
    }

    #[test]
    fn adopt_previous_only_with_legacy() {
        assert!(!chooser(1).controls().contains(&Control::AdoptLegacy));
        let mut legacy = LayoutChooser::new(false, true, 1, Vec::new());
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
        let shapes = grid_shapes(5);
        let labels: Vec<(usize, &str)> = shapes
            .iter()
            .map(|shape| (shape.cols, shape.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            vec![(0, "Auto"), (2, "2 × 3"), (3, "3 × 2"), (4, "4 × 2")],
            "Auto names no count: the daemon fits it to the pane area"
        );
        assert_eq!(grid_shapes(2).len(), 1, "no shape between 1 and N columns");

        let mut model = chooser(5);
        assert!(!model.controls().contains(&Control::Shape(0)), "collapsed");
        model.press(&Control::OpenAll, ASPECT);
        assert!(
            model.controls().contains(&Control::Shape(4)),
            "grid is the default"
        );
        assert_eq!(model.label(&Control::Shape(0), ASPECT), "Auto");
        model.press(&Control::Mode(Mode::SideBySide), ASPECT);
        assert!(
            !model.controls().contains(&Control::Shape(0)),
            "shapes are grid-only"
        );
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
            layout: RearrangeLayout::Grid {
                cols: 0,
                aspect: Some(ASPECT),
            },
            max_per_tab: None,
        };
        assert!(
            matches!(&auto, Some((InitLayoutKind::AllSessions, Some(a))) if *a == expected),
            "{auto:?} is the automatic grid with the pane area's shape"
        );
        model.press(&Control::Shape(2), ASPECT);
        model.press(&Control::MaxUp, ASPECT);
        let chosen = model.press(&Control::Confirm, ASPECT);
        let expected = Arrangement {
            layout: RearrangeLayout::Grid {
                cols: 2,
                aspect: None,
            },
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
        let layout = RearrangeLayout::Grid {
            cols: 2,
            aspect: None,
        };
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
                    request_id: None,
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
        let mut model = LayoutChooser::new(false, true, 3, vec![clonable("c1", None)]);
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
