//! The first-connect layout chooser: its modal, its keys, and the
//! arrangement it chose laid over the daemon's answer. The model is
//! [`crate::layout_chooser`].

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, FontWeight, Keystroke, SharedString, Stateful,
    Window, div, prelude::*, px,
};
use protocol::{ClientMessage, ClonableLayout, TabEntry};

use crate::layout_chooser::{Control, FALLBACK_ASPECT, LayoutChooser, Mode, arrangement_messages};
use crate::notice_view::modal_panel;
use crate::session_menu::{backdrop, dialog_button};
use crate::tabs::collect_panes;
use crate::{BORDER, HOVER_BG, MUTED, RootView, TEXT};

const TITLE: &str = "Set up this window";
const INTRO: &str = "This is a new client. Sessions are shared with every connected window, but each curates its own tabs. How should this window start?";
const LAYOUT_LABEL: &str = "Layout";
const MAX_LABEL: &str = "Max sessions per tab";
const SELECTED_BG: u32 = 0x0037_3a44;

impl RootView {
    /// Whether the first-connect layout chooser is open.
    #[must_use]
    pub fn layout_chooser_open(&self) -> bool {
        self.layout_chooser.is_some()
    }

    /// The open chooser's controls in the keyboard's order: selector and
    /// label.
    #[must_use]
    pub fn layout_chooser_controls(&self) -> Vec<(String, String)> {
        let Some(chooser) = &self.layout_chooser else {
            return Vec::new();
        };
        let aspect = self.pane_area_aspect();
        chooser
            .controls()
            .iter()
            .map(|control| (control.selector(), chooser.label(control, aspect)))
            .collect()
    }

    /// The daemon's `LayoutInitRequired`: the chooser opens on Start empty
    /// and the client waits for a choice.
    pub(crate) fn open_layout_chooser(
        &mut self,
        has_legacy: bool,
        active_session_count: u32,
        clonable: Vec<ClonableLayout>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(
            has_legacy,
            active_session_count,
            clonable = clonable.len(),
            "first connect for this client: asking how to lay it out"
        );
        let count = usize::try_from(active_session_count).unwrap_or(usize::MAX);
        self.layout_chooser = Some(LayoutChooser::new(has_legacy, count, clonable));
        if self.exit.is_none() {
            self.chooser_focus.focus(window);
        }
        cx.notify();
    }

    /// Closes the chooser, when it is open, and hands the keyboard back.
    pub(crate) fn close_layout_chooser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout_chooser.take().is_none() {
            return;
        }
        if self.exit.is_none() {
            self.after_notice_closed(window, cx);
        }
        cx.notify();
    }

    /// A key while the chooser is open; returns whether it was. Tab and
    /// Shift+Tab move the focus, Enter and Space press the focused
    /// control, and every other key, Esc included, does nothing.
    pub(crate) fn on_layout_chooser_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(chooser) = &mut self.layout_chooser else {
            return false;
        };
        match keystroke.key.as_str() {
            "tab" => {
                chooser.move_focus(!keystroke.modifiers.shift);
                cx.notify();
            }
            "enter" | "space" => {
                let control = chooser.focused();
                self.press_layout_control(&control, window, cx);
            }
            _ => {}
        }
        true
    }

    /// Presses `control`; a choice goes to the daemon and closes the
    /// chooser, keeping the arrangement for the daemon's answer.
    fn press_layout_control(
        &mut self,
        control: &Control,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let aspect = self.pane_area_aspect();
        let Some(chooser) = &mut self.layout_chooser else {
            return;
        };
        if let Some((kind, arrangement)) = chooser.press(control, aspect) {
            tracing::info!(?kind, ?arrangement, "layout chooser: chosen");
            self.send(ClientMessage::InitLayout { kind });
            self.pending_arrangement = arrangement;
            self.close_layout_chooser(window, cx);
        }
        cx.notify();
    }

    /// The daemon's whole tab list: the chooser closes, and the
    /// arrangement it chose, if any, lays the first tab's panes out once.
    pub(crate) fn on_tab_list(
        &mut self,
        tabs: &[TabEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_layout_chooser(window, cx);
        let Some(arrangement) = self.pending_arrangement.take() else {
            return;
        };
        let Some((tab, grid)) = tabs
            .first()
            .and_then(|tab| tab.grid().map(|grid| (tab, grid)))
        else {
            tracing::info!("layout chooser: no grid tab to arrange");
            return;
        };
        let pane_ids: Vec<String> = collect_panes(grid)
            .iter()
            .map(|pane| pane.id.to_owned())
            .collect();
        for msg in arrangement_messages(&tab.id, &pane_ids, arrangement) {
            self.send(msg);
        }
    }

    /// The pane area's width over its height, or 16:10 before it has been
    /// laid out.
    fn pane_area_aspect(&self) -> f32 {
        self.grid_bounds
            .map(|bounds| bounds.size.width / bounds.size.height)
            .filter(|aspect| aspect.is_finite() && *aspect > 0.0)
            .unwrap_or(FALLBACK_ASPECT)
    }

    /// The chooser over a backdrop that takes every click beneath it.
    pub(crate) fn layout_chooser_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let chooser = self.layout_chooser.as_ref()?;
        let aspect = self.pane_area_aspect();
        let focused = chooser.focused();
        let mut options: Vec<AnyElement> = Vec::new();
        for control in chooser.controls() {
            match control {
                Control::StartEmpty
                | Control::OpenAll
                | Control::AdoptLegacy
                | Control::Clone(_) => {
                    options.push(option_row(chooser, &control, &focused, aspect, cx));
                    if control == Control::OpenAll && chooser.expanded() {
                        options.push(picker(chooser, &focused, aspect, cx).into_any_element());
                    }
                }
                _ => {}
            }
        }
        let panel = modal_panel("layout-chooser-panel")
            .track_focus(&self.chooser_focus)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(div().text_color(gpui::rgb(MUTED)).child(INTRO))
            .child(div().flex().flex_col().gap(px(6.0)).children(options));
        Some(backdrop("layout-chooser", panel))
    }
}

/// An option of the chooser: its name over its muted detail, filled while
/// its picker is open and outlined while focused.
fn option_row(
    chooser: &LayoutChooser,
    control: &Control,
    focused: &Control,
    aspect: f32,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let name = control.selector();
    let pressed = control.clone();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if control == focused { TEXT } else { BORDER }))
        .when(chooser.is_selected(control), |row| {
            row.bg(gpui::rgb(SELECTED_BG))
        })
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.press_layout_control(&pressed, window, cx);
        }))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(chooser.label(control, aspect)),
        )
        .children(
            chooser
                .detail(control)
                .map(|detail| div().text_color(gpui::rgb(MUTED)).child(detail)),
        )
        .into_any_element()
}

/// A small button of the picker: filled when chosen, outlined when focused.
fn picker_button(
    chooser: &LayoutChooser,
    control: &Control,
    focused: &Control,
    aspect: f32,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let pressed = control.clone();
    dialog_button(
        &control.selector(),
        chooser.label(control, aspect),
        false,
        control == focused,
    )
    .when(chooser.is_selected(control), |button| {
        button.bg(gpui::rgb(SELECTED_BG))
    })
    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_layout_control(&pressed, window, cx);
    }))
}

/// The "Open all active sessions" picker: the layout modes with the grid
/// shapes under Grid, the max-per-tab stepper and the confirm.
fn picker(
    chooser: &LayoutChooser,
    focused: &Control,
    aspect: f32,
    cx: &mut Context<RootView>,
) -> Div {
    let mut button = |control: &Control| picker_button(chooser, control, focused, aspect, cx);
    let grid = button(&Control::Mode(Mode::Grid));
    let shapes: Vec<Stateful<Div>> = chooser
        .controls()
        .iter()
        .filter(|control| matches!(control, Control::Shape(_)))
        .map(&mut button)
        .collect();
    let side = button(&Control::Mode(Mode::SideBySide));
    let stacked = button(&Control::Mode(Mode::Stacked));
    let down = button(&Control::MaxDown);
    let up = button(&Control::MaxUp);
    let confirm = button(&Control::Confirm);
    let modes = div()
        .flex()
        .flex_col()
        .items_start()
        .gap(px(4.0))
        .child(div().text_color(gpui::rgb(MUTED)).child(LAYOUT_LABEL))
        .child(grid)
        .when(!shapes.is_empty(), |modes| {
            modes.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(4.0))
                    .pl(px(12.0))
                    .children(shapes),
            )
        })
        .child(side)
        .child(stacked);
    let max = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(div().text_color(gpui::rgb(MUTED)).child(MAX_LABEL))
        .child(down)
        .child(
            div()
                .debug_selector(|| "chooser-max-value".to_owned())
                .child(chooser.max_label()),
        )
        .child(up);
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .pl(px(12.0))
        .child(modes)
        .child(max)
        .child(div().flex().justify_end().child(confirm))
}
