//! The first-connect layout chooser: its modal, its keys, and the
//! arrangement it chose laid over the daemon's answer; and the import modal
//! a tailnet host's changed session count opens, which shares its picker.
//! The models are in [`crate::layout_chooser`].

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, FontWeight, Keystroke, SharedString, Stateful,
    Window, div, prelude::*, px,
};
use protocol::{ClientMessage, ClonableLayout, DaemonMessage, TabEntry};

use crate::RootView;
use crate::layout_chooser::{
    Control, FALLBACK_ASPECT, ImportChooser, ImportControl, ImportOffer, ImportOutcome,
    LayoutChooser, Mode, arrangement_messages, import_intro, import_offer,
};
use crate::notice_view::modal_panel;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::spawn_view::close_button;
use crate::tabs::collect_panes;
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};

const TITLE: &str = "Set up this window";
const INTRO: &str = "This is a new client. Sessions are shared with every connected window, but each curates its own tabs. How should this window start?";
const LAYOUT_LABEL: &str = "Layout";
const MAX_LABEL: &str = "Max sessions per tab";
const IMPORT_TITLE: &str = "Import remote sessions";
const IMPORT_REPLACES: &str = "Opening them or starting empty replaces this window's tabs on this host. Sessions keep running.";

impl RootView {
    /// Whether the first-connect layout chooser is open.
    #[must_use]
    pub fn layout_chooser_open(&self) -> bool {
        self.layout_chooser.is_some()
    }

    /// The open chooser's controls in the keyboard's order: selector and
    /// label; while the import modal is open, its controls, the picker's
    /// among them.
    #[must_use]
    pub fn layout_chooser_controls(&self) -> Vec<(String, String)> {
        let aspect = self.pane_area_aspect();
        if let Some(import) = &self.import_modal {
            return import
                .controls()
                .iter()
                .map(|control| (control.selector(), import.label(control, aspect)))
                .collect();
        }
        let Some(chooser) = &self.layout_chooser else {
            return Vec::new();
        };
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
            self.send_to(&HostKey::Local, ClientMessage::InitLayout { kind });
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
            self.send_to(&HostKey::Local, msg);
        }
    }

    /// The main window's pane area's width over its height, or 16:10 before
    /// it has been laid out.
    pub(crate) fn pane_area_aspect(&self) -> f32 {
        self.layouts
            .main_grid()
            .map(|bounds| bounds.size.width / bounds.size.height)
            .filter(|aspect| aspect.is_finite() && *aspect > 0.0)
            .unwrap_or(FALLBACK_ASPECT)
    }

    /// The chooser over a backdrop that takes every click beneath it.
    pub(crate) fn layout_chooser_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
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
                        let picker = picker(chooser, Some(&focused), aspect, cx);
                        options.push(picker.into_any_element());
                    }
                }
                _ => {}
            }
        }
        let panel = modal_panel(
            crate::fonts::ui_text_size(cx),
            "layout-chooser-panel",
            &theme(cx),
        )
        .track_focus(&self.chooser_focus)
        .child(
            div()
                .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                .child(TITLE),
        )
        .child(div().text_color(gpui::rgb(t.muted)).child(INTRO))
        .child(div().flex().flex_col().gap(px(6.0)).children(options));
        Some(backdrop("layout-chooser", panel, &theme(cx)))
    }

    /// A press of a picker control, in whichever modal shows the picker.
    fn press_picker_control(
        &mut self,
        control: &Control,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.import_modal.is_some() {
            self.press_import_control(&ImportControl::Picker(control.clone()), window, cx);
        } else {
            self.press_layout_control(control, window, cx);
        }
    }

    /// Whether the import modal is open.
    #[must_use]
    pub fn import_modal_open(&self) -> bool {
        self.import_modal.is_some()
    }

    /// What a daemon message means for the import offer: a remote host's
    /// first session list after `Welcome` is compared with the count
    /// recorded for the host; every session list on a remote host then
    /// records its count; `LayoutInitRequired` drops the offer for the
    /// chooser; the next tab list opens it unless it answers our own
    /// arrangement; a new or removed session records the host's live count.
    pub(crate) fn on_import_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match msg {
            DaemonMessage::Sessions { sessions } => {
                let initial = std::mem::take(&mut self.awaiting_initial_sessions);
                let host = self.host_count_key();
                let recorded = host
                    .as_deref()
                    .and_then(|host| self.sidebar.host_session_count(host));
                let offer = import_offer(host.is_some(), initial, sessions.len(), recorded);
                if offer.is_some() {
                    self.pending_import = offer;
                }
                if let Some(host) = &host
                    && self.record_host_session_count(host, sessions.len())
                {
                    self.save_ui();
                }
            }
            DaemonMessage::LayoutInitRequired { .. } => self.pending_import = None,
            DaemonMessage::Tabs { .. } => {
                if let Some(offer) = self.pending_import.take()
                    && self.pending_arrangement.is_none()
                {
                    self.open_import_modal(offer, window, cx);
                }
            }
            DaemonMessage::SessionUpdated { .. } | DaemonMessage::SessionRemoved { .. } => {
                if !self.awaiting_initial_sessions
                    && let Some(host) = self.host_count_key()
                    && self.record_host_session_count(&host, self.seen_count(&HostKey::Local))
                {
                    self.schedule_ui_save(cx);
                }
            }
            _ => {}
        }
    }

    /// The connected remote host's session-count key; `None` on this PC.
    fn host_count_key(&self) -> Option<String> {
        self.conn().target().host_key()
    }

    /// Records `count` under `host`, the connected host's session-count
    /// key, while that host is a saved host, so a forgotten one records
    /// nothing; returns whether the recorded count changed, so the caller
    /// saves.
    fn record_host_session_count(&mut self, host: &str, count: usize) -> bool {
        let target = self.conn().target();
        let saved = self
            .sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .any(|saved| saved.is_for(target));
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        saved && self.sidebar.set_host_session_count(host, count)
    }

    /// Whether the import modal holds the keyboard.
    #[must_use]
    pub fn import_modal_focused(&self, window: &Window) -> bool {
        self.import_modal.is_some() && self.chooser_focus.is_focused(window)
    }

    /// Opens the import modal for `offer`, on Keep.
    pub(crate) fn open_import_modal(
        &mut self,
        offer: ImportOffer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(
            incoming = offer.incoming,
            recorded = offer.recorded,
            "the remote host's session count changed: offering the import"
        );
        self.import_modal = Some(ImportChooser::new(offer));
        if self.exit.is_none() {
            self.chooser_focus.focus(window);
        }
        cx.notify();
    }

    /// Closes the import modal, when it is open, and hands the keyboard
    /// back.
    pub(crate) fn close_import_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.import_modal.take().is_none() {
            return;
        }
        if self.exit.is_none() {
            self.after_notice_closed(window, cx);
        }
        cx.notify();
    }

    /// A new connection: no offer held, no first list awaited, and the
    /// modal closed.
    pub(crate) fn reset_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_import = None;
        self.awaiting_initial_sessions = false;
        self.close_import_modal(window, cx);
    }

    /// A key while the import modal is open; returns whether it was. Tab
    /// and Shift+Tab move the focus, Enter and Space press the focused
    /// control, and Esc keeps the layout.
    pub(crate) fn on_import_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(import) = &mut self.import_modal else {
            return false;
        };
        match keystroke.key.as_str() {
            "tab" => {
                import.move_focus(!keystroke.modifiers.shift);
                cx.notify();
            }
            "enter" | "space" => {
                let control = import.focused();
                self.press_import_control(&control, window, cx);
            }
            "escape" => self.press_import_control(&ImportControl::Keep, window, cx),
            _ => {}
        }
        true
    }

    /// Presses `control`; Keep closes the modal and sends nothing, Open all
    /// and Start empty go to the daemon, keeping the arrangement for its
    /// answer.
    fn press_import_control(
        &mut self,
        control: &ImportControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let aspect = self.pane_area_aspect();
        let Some(import) = &mut self.import_modal else {
            return;
        };
        match import.press(control, aspect) {
            Some(ImportOutcome::Keep) => {
                tracing::info!("import: keeping the layout");
                self.close_import_modal(window, cx);
            }
            Some(ImportOutcome::Send((kind, arrangement))) => {
                tracing::info!(?kind, ?arrangement, "import: chosen");
                self.send_to(&HostKey::Local, ClientMessage::InitLayout { kind });
                self.pending_arrangement = arrangement;
                self.close_import_modal(window, cx);
            }
            None => {}
        }
        cx.notify();
    }

    /// The import modal over a backdrop that takes every click beneath it.
    pub(crate) fn import_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        let import = self.import_modal.as_ref()?;
        let aspect = self.pane_area_aspect();
        let focused = import.focused();
        let focused_picker = match &focused {
            ImportControl::Picker(control) => Some(control),
            _ => None,
        };
        let mut options: Vec<AnyElement> = Vec::new();
        for control in [
            ImportControl::Keep,
            ImportControl::OpenAll,
            ImportControl::Empty,
        ] {
            let pressed = control.clone();
            let row = option_row_element(
                crate::fonts::ui_weight_shift(cx),
                control.selector(),
                import.label(&control, aspect),
                control.detail(),
                RowMarks {
                    focused: control == focused,
                    selected: import.is_selected(&control),
                },
                &t,
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.press_import_control(&pressed, window, cx);
            }));
            options.push(row.into_any_element());
            if control == ImportControl::OpenAll && import.expanded() {
                let picker = picker(import.picker(), focused_picker, aspect, cx);
                options.push(picker.into_any_element());
            }
        }
        let close = close_button("import-close", false, &theme(cx)).on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| {
                this.press_import_control(&ImportControl::Keep, window, cx);
            },
        ));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(IMPORT_TITLE),
            )
            .child(close);
        let panel = modal_panel(crate::fonts::ui_text_size(cx), "import-panel", &theme(cx))
            .track_focus(&self.chooser_focus)
            .child(header)
            .child(
                div()
                    .text_color(gpui::rgb(t.muted))
                    .child(import_intro(import.offer())),
            )
            .child(div().text_color(gpui::rgb(t.muted)).child(IMPORT_REPLACES))
            .child(div().flex().flex_col().gap(px(6.0)).children(options));
        Some(backdrop("import-modal", panel, &theme(cx)))
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
    let pressed = control.clone();
    option_row_element(
        crate::fonts::ui_weight_shift(cx),
        control.selector(),
        chooser.label(control, aspect),
        chooser.detail(control),
        RowMarks {
            focused: control == focused,
            selected: chooser.is_selected(control),
        },
        &theme(cx),
    )
    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_layout_control(&pressed, window, cx);
    }))
    .into_any_element()
}

/// How an option row is marked: outlined while focused, filled while
/// selected.
struct RowMarks {
    focused: bool,
    selected: bool,
}

/// An option row tagged `selector`: `label` over its muted `detail`,
/// filled while `selected` and outlined while `focused`; the caller adds
/// the click.
fn option_row_element(
    shift: i8,
    selector: String,
    label: String,
    detail: Option<String>,
    RowMarks { focused, selected }: RowMarks,
    t: &UiTheme,
) -> Stateful<Div> {
    div()
        .id(ElementId::Name(SharedString::from(selector.clone())))
        .debug_selector(|| selector)
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focused { t.text } else { t.border }))
        .when(selected, |row| row.bg(gpui::rgb(t.selected_bg)))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(t.popover_hover)))
        .child(
            div()
                .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                .child(label),
        )
        .children(detail.map(|detail| div().text_color(gpui::rgb(t.muted)).child(detail)))
}

/// A small button of the picker: filled when chosen, ringed when focused.
fn picker_button(
    chooser: &LayoutChooser,
    control: &Control,
    focused: Option<&Control>,
    aspect: f32,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let t = theme(cx);
    let pressed = control.clone();
    dialog_button(
        &control.selector(),
        chooser.label(control, aspect),
        dialog_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            false,
            true,
            &theme(cx),
        ),
        focused == Some(control),
    )
    .when(chooser.is_selected(control), |button| {
        button.bg(gpui::rgb(t.selected_bg))
    })
    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_picker_control(&pressed, window, cx);
    }))
}

/// The "Open all active sessions" picker: the layout modes with the grid
/// shapes under Grid, the max-per-tab stepper and the confirm.
fn picker(
    chooser: &LayoutChooser,
    focused: Option<&Control>,
    aspect: f32,
    cx: &mut Context<RootView>,
) -> Div {
    let t = theme(cx);
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
        .child(div().text_color(gpui::rgb(t.muted)).child(LAYOUT_LABEL))
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
        .child(div().text_color(gpui::rgb(t.muted)).child(MAX_LABEL))
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
