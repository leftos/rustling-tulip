//! The undo shelf on screen: what each entry says, the timer that expires
//! it, and what its buttons send. The model is [`crate::undo`].

use std::collections::HashSet;

use gpui::{AnyElement, ClickEvent, Context, ElementId, SharedString, Window, div, prelude::*, px};

use crate::session_menu::dialog_button;
use crate::undo::{self, TabSnapshot, UndoEntry};
use crate::{BORDER, FOOTER_HEIGHT, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, tooltip};

/// The widest the shelf gets; a narrower window narrows it.
const SHELF_WIDTH: f32 = 460.0;
/// What the shelf leaves free at each side of the window.
const SHELF_MARGIN: f32 = 16.0;

impl RootView {
    /// The undo entries on screen, newest first.
    #[must_use]
    pub fn undo_entries(&self) -> &[UndoEntry] {
        self.undo.entries()
    }

    /// Shows `message` for `snapshots`, newest first, and arms its timer.
    pub(crate) fn record_undo(
        &mut self,
        message: String,
        snapshots: Vec<TabSnapshot>,
        cx: &mut Context<Self>,
    ) {
        if snapshots.is_empty() {
            return;
        }
        self.undo.push(message, snapshots, (self.now)());
        self.schedule_undo_expiry(cx);
        cx.notify();
    }

    /// `tab_id` as it stands, focusing `pane_id` when it comes back; none
    /// for a tab the daemon no longer lists.
    pub(crate) fn tab_snapshot(
        &self,
        tab_id: &str,
        focus_pane: Option<&str>,
    ) -> Option<TabSnapshot> {
        let index = self.tabs.tabs().iter().position(|tab| tab.id == tab_id)?;
        Some(TabSnapshot {
            tab: self.tabs.tab(tab_id)?.clone(),
            index,
            restore_active: self.tabs.active_id() == Some(tab_id),
            focus_pane: focus_pane.map(str::to_owned),
        })
    }

    /// The entry for closing the pane `pane_id` of `tab_id`, whose session
    /// shows as `label`; an empty pane has none.
    pub(crate) fn record_pane_close(
        &mut self,
        tab_id: &str,
        pane_id: &str,
        label: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = self.tab_snapshot(tab_id, Some(pane_id)) else {
            return;
        };
        let message = label.map_or_else(
            || undo::CLOSED_EMPTY_PANE.to_owned(),
            undo::closed_pane_message,
        );
        self.record_undo(message, vec![snapshot], cx);
    }

    /// Every entry goes: a fresh or lost connection must empty the shelf.
    pub(crate) fn reset_undo(&mut self, cx: &mut Context<Self>) {
        self.undo.clear();
        self.schedule_undo_expiry(cx);
    }

    /// The Undo button of entry `id`: its snapshots go back in ascending
    /// index order, and one that was the tab shown takes the keyboard again.
    fn press_undo(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.undo.take(id) else {
            return;
        };
        self.schedule_undo_expiry(cx);
        // A tab whose removal is on its way counts as gone: the daemon
        // removes it before the restore lands, so only a `RestoreTab` can
        // put it back.
        let live = undo::live_tabs(self.tabs.tabs(), self.tabs.closing());
        let known: HashSet<String> = self
            .sidebar
            .sessions()
            .iter()
            .map(|session| session.id.clone())
            .collect();
        let messages = undo::restore_messages(&entry, &live, &known);
        for snapshot in entry.ordered() {
            if !snapshot.restore_active {
                continue;
            }
            let tab_id = snapshot.tab.id.as_str();
            let focus = snapshot.focus_pane.as_deref();
            if live.contains(tab_id) {
                match focus {
                    Some(pane) => self.tabs.focus_pane(tab_id, pane),
                    None => self.tabs.activate(tab_id),
                }
            } else {
                // A tab that is gone comes back at its id: only that id
                // activates it, so a refused restore arms nothing.
                self.tabs.activate_on_arrival(tab_id, focus);
            }
        }
        for msg in messages {
            self.send(msg);
        }
        self.after_tabs_change(window, cx);
        cx.notify();
    }

    /// The ✕ on entry `id`, which goes without anything being sent.
    fn dismiss_undo(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.undo.dismiss(id) {
            self.schedule_undo_expiry(cx);
            cx.notify();
        }
    }

    /// Arms a timer for the next entry to go; replacing the timer cancels
    /// the one before.
    fn schedule_undo_expiry(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        self.undo_timer = self.undo.next_expiry().map(|deadline| {
            let delay = deadline.saturating_duration_since(now);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                // Fails only when the view is gone, and the shelf with it.
                this.update(cx, Self::expire_undo).ok();
            })
        });
    }

    /// The timer fired: drop the entries whose time is up by the clock, which
    /// may lag the timer, and wait for the next.
    fn expire_undo(&mut self, cx: &mut Context<Self>) {
        if self.undo.expire((self.now)()) {
            cx.notify();
        }
        self.schedule_undo_expiry(cx);
    }

    /// The shelf, bottom centre above the footer. Only its cards take a
    /// press, and nothing of it takes the keyboard.
    pub(crate) fn undo_layer(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.undo.is_empty() {
            return None;
        }
        let width = (window.viewport_size().width / px(1.0) - 2.0 * SHELF_MARGIN).min(SHELF_WIDTH);
        let accent = self.sidebar.appearance(None).accent.value;
        let cards: Vec<AnyElement> = self
            .undo
            .entries()
            .iter()
            .map(|entry| undo_card(entry, accent, cx))
            .collect();
        Some(
            div()
                .absolute()
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(FOOTER_HEIGHT + 8.0))
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("undo-shelf")
                        .debug_selector(|| "undo-shelf".to_owned())
                        .w(px(width))
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .children(cards),
                )
                .into_any_element(),
        )
    }
}

/// One entry's card: what it says, its Undo button and its ✕. The Undo
/// button wears the app's accent.
fn undo_card(entry: &UndoEntry, accent: u32, cx: &mut Context<RootView>) -> AnyElement {
    let id = entry.id;
    let action = dialog_button(
        &format!("undo-action-{id}"),
        undo::UNDO_LABEL.to_owned(),
        false,
        false,
    )
    .flex_none()
    .h(px(28.0))
    .text_color(gpui::rgb(accent))
    .border_color(gpui::rgb(accent))
    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_undo(id, window, cx);
    }));
    let dismiss = dialog_button(&format!("undo-dismiss-{id}"), "✕".to_owned(), false, false)
        .flex_none()
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .tooltip(tooltip("Dismiss undo"))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.dismiss_undo(id, cx);
        }));
    let message = div()
        .flex_1()
        .min_w(px(0.0))
        .truncate()
        .child(entry.message.clone());
    let name = format!("undo-entry-{id}");
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(8.0))
        .p(px(10.0))
        .min_h(px(42.0))
        .bg(gpui::rgb(PANEL_BG))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .rounded(px(6.0))
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(TEXT))
        .occlude()
        .child(message)
        .child(action)
        .child(dismiss)
        .into_any_element()
}
