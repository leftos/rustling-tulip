//! The tab strip: a pill per tab (click to show it, double-click to rename,
//! middle-click or × to close, twice when the tab holds something), then "+"
//! for a new tab.

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, Entity, Focusable as _, MouseButton,
    MouseDownEvent, SharedString, Stateful, Subscription, Window, div, prelude::*, px,
};
use protocol::{ClientMessage, TabContent, TabEntry};

use crate::text_input::{TextInput, TextInputEvent};
use crate::{
    BAR_BG, BORDER, DANGER, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, tooltip,
};

const TAB_BAR_HEIGHT: f32 = 26.0;
const RENAME_WIDTH: f32 = 140.0;

/// A tab name being edited in place.
pub(crate) struct Rename {
    tab_id: String,
    input: Entity<TextInput>,
    _subscriptions: [Subscription; 2],
}

impl Rename {
    pub(crate) fn tab_id(&self) -> &str {
        &self.tab_id
    }
}

/// How a rename ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameEnd {
    Submit,
    Cancel,
    /// The input lost the keyboard to something the user chose.
    Blur,
}

impl RootView {
    pub(crate) fn tab_bar(&self, cx: &mut Context<Self>) -> Div {
        let pills: Vec<AnyElement> = self
            .tabs
            .tabs()
            .iter()
            .map(|tab| self.tab_pill(tab, cx))
            .collect();
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(TAB_BAR_HEIGHT))
            .bg(gpui::rgb(BAR_BG))
            .border_b_1()
            .border_color(gpui::rgb(BORDER))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(MUTED))
            .children(pills)
            .child(new_tab_button(cx))
    }

    fn tab_pill(&self, tab: &TabEntry, cx: &mut Context<Self>) -> AnyElement {
        let active = self.tabs.active_id() == Some(tab.id.as_str());
        let armed = self.tabs.close_confirm.armed() == Some(tab.id.as_str());
        let rename = self.renaming.as_ref().filter(|r| r.tab_id == tab.id);
        let label = match rename {
            Some(rename) => div().w(px(RENAME_WIDTH)).child(rename.input.clone()),
            None => div().whitespace_nowrap().child(pill_label(tab)),
        };
        let (click_id, middle_id) = (tab.id.clone(), tab.id.clone());
        let on_click = cx.listener(move |this, event: &ClickEvent, window, cx| {
            this.click_tab(&click_id, event.click_count(), window, cx);
        });
        let on_middle = cx.listener(move |this, _: &MouseDownEvent, _, cx| {
            this.close_tab_click(&middle_id);
            cx.stop_propagation();
            cx.notify();
        });
        let name = format!("tab-{}", tab.id);
        div()
            .id(ElementId::Name(SharedString::from(name.clone())))
            .debug_selector(|| name)
            .flex()
            .items_center()
            .gap(px(6.0))
            .h_full()
            .px(px(10.0))
            .border_r_1()
            .border_color(gpui::rgb(BORDER))
            .cursor_pointer()
            .when(active, |pill| {
                pill.bg(gpui::rgb(PANEL_BG)).text_color(gpui::rgb(TEXT))
            })
            .when(!active, |pill| {
                pill.hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            })
            .when(rename.is_none(), move |pill| {
                pill.on_click(on_click)
                    .on_mouse_down(MouseButton::Middle, on_middle)
            })
            .child(label)
            .child(close_button(&tab.id, armed, cx))
            .into_any_element()
    }

    fn click_tab(
        &mut self,
        tab_id: &str,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if click_count >= 2 {
            self.start_rename(tab_id, window, cx);
        } else {
            self.tabs.activate(tab_id);
            self.after_tabs_change(window, cx);
        }
    }

    fn new_tab(&mut self) {
        self.tabs.arm_create();
        self.send(ClientMessage::CreateTab {
            name: None,
            initial_session_id: None,
        });
    }

    /// A close click: closes the tab, or arms the close when the tab holds
    /// a session or a split.
    fn close_tab_click(&mut self, tab_id: &str) {
        let Some(tab) = self.tabs.tab(tab_id).cloned() else {
            return;
        };
        if self.tabs.close_confirm.click(&tab) {
            self.send(ClientMessage::CloseTab { tab_id: tab.id });
        }
    }

    fn start_rename(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.tabs.tab(tab_id).map(|tab| tab.name.clone()) else {
            return;
        };
        let input = cx.new(|cx| TextInput::new(name, "Tab name", cx));
        let events = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &TextInputEvent, window, cx| {
                let end = match event {
                    TextInputEvent::Submit => RenameEnd::Submit,
                    TextInputEvent::Cancel => RenameEnd::Cancel,
                };
                this.finish_rename(end, window, cx);
            },
        );
        let handle = input.read(cx).focus_handle(cx);
        let blur = cx.on_blur(&handle, window, |this, window, cx| {
            this.finish_rename(RenameEnd::Blur, window, cx);
        });
        handle.focus(window);
        self.renaming = Some(Rename {
            tab_id: tab_id.to_owned(),
            input,
            _subscriptions: [events, blur],
        });
        cx.notify();
    }

    /// Ends a rename. Enter and a blur keep a changed, non-blank name; Enter
    /// and Esc hand the keyboard back to the active tab's pane.
    fn finish_rename(&mut self, end: RenameEnd, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.renaming.take() else {
            return;
        };
        let name = rename.input.read(cx).text().trim().to_owned();
        let changed = self
            .tabs
            .tab(&rename.tab_id)
            .is_some_and(|tab| tab.name != name);
        if end != RenameEnd::Cancel && changed && !name.is_empty() {
            self.send(ClientMessage::RenameTab {
                tab_id: rename.tab_id,
                name,
            });
        }
        if end != RenameEnd::Blur {
            self.focus_active_pane(window, cx);
        }
        cx.notify();
    }
}

/// A tab's name; a diff tab's is marked Δ.
fn pill_label(tab: &TabEntry) -> String {
    match tab.content {
        TabContent::Diff { .. } => format!("Δ {}", tab.name),
        TabContent::Grid { .. } => tab.name.clone(),
    }
}

/// The ×, or ✓ while a close waits for its second click. It acts on the
/// press so the root's click-elsewhere reset never sees it.
fn close_button(tab_id: &str, armed: bool, cx: &mut Context<RootView>) -> Stateful<Div> {
    let id = tab_id.to_owned();
    let tip = if armed {
        "Click again to confirm closing this tab"
    } else {
        "Close tab"
    };
    let name = format!("tab-close-{tab_id}");
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .px(px(3.0))
        .rounded(px(3.0))
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .when(armed, |button| button.text_color(gpui::rgb(DANGER)))
        .tooltip(tooltip(tip))
        .child(if armed { "✓" } else { "×" })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                this.close_tab_click(&id);
                cx.stop_propagation();
                cx.notify();
            }),
        )
}

fn new_tab_button(cx: &mut Context<RootView>) -> Stateful<Div> {
    div()
        .id("new-tab")
        .debug_selector(|| "new-tab".to_owned())
        .px(px(10.0))
        .h_full()
        .flex()
        .items_center()
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
        .tooltip(tooltip("New tab"))
        .child("+")
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.new_tab();
            cx.notify();
        }))
}
