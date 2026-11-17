//! Draws the phone's Settings Keys page ([`super::settings`]): a muted hint,
//! the key grid in fixed equal columns, and Restore defaults with its confirm
//! row. It only renders and forwards taps.

use gpui::{ClickEvent, Context, Div, Stateful, Window, div, prelude::*, px};

use super::PhoneRoot;
use super::home_view::{SIDE_PADDING, TOUCH_TARGET};
use super::hosts_view::phone_button;
use super::key_grid::{self, GridKey};
use super::settings::Page;
use crate::buttons::{ButtonKind, ButtonSize, Layer, outlined_button};
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::ui_theme::UiTheme;

/// The line over the grid, naming the two gestures the page answers.
const HINT: &str = "Long-press a key to move it; tap one to change it.";
/// The Restore defaults button's label.
const RESTORE_ROW_LABEL: &str = "Restore defaults";
/// The confirm row's Restore.
const RESTORE_LABEL: &str = "Restore";
/// The question the Restore defaults button turns into.
const RESTORE_ASK: &str = "Replace these with the defaults?";
/// The confirm row's Cancel.
const CANCEL_LABEL: &str = "Cancel";
/// The least width of a grid column, in points.
const CELL_MIN: f32 = 72.0;
/// The gap between two cells, in points.
const CELL_GAP: f32 = 8.0;

/// What the Keys page's Restore defaults draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeysRestore {
    /// The button.
    Idle,
    /// The question row, with its Restore and Cancel.
    Asking,
}

/// A button tagged `selector`, drawn dimmed and taking no tap while `enabled`
/// is false.
fn restore_button(
    selector: &str,
    enabled: bool,
    label: &'static str,
    t: &UiTheme,
    cx: &Context<PhoneRoot>,
) -> Stateful<Div> {
    outlined_button(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        selector,
        ButtonSize::Large,
        enabled,
        Layer::Base,
        t,
    )
    .min_h(px(TOUCH_TARGET))
    .child(label)
}

impl PhoneRoot {
    /// Opens Settings on its Keys page, as the shot example does.
    pub fn open_settings_keys(&mut self, cx: &mut Context<Self>) {
        self.open_settings_on(Page::Keys, cx);
    }

    /// Keeps the Keys page's Restore confirm only while that page tops
    /// Settings: leaving the page, opening anything over it or closing
    /// Settings drops it.
    pub(super) fn keep_keys_restore_confirm(&mut self) {
        let on_page = self
            .settings
            .as_ref()
            .is_some_and(|settings| settings.top().is_none() && settings.page == Some(Page::Keys));
        if !on_page {
            self.keys_restore = KeysRestore::Idle;
        }
    }

    /// The grid the page draws: the saved one, else the built-in grid.
    fn saved_keys(&self, cx: &Context<Self>) -> Option<Vec<GridKey>> {
        self.root.read(cx).sidebar.ui_state().phone_keys.clone()
    }

    /// A tap on Restore defaults: the confirm row asks.
    fn ask_keys_restore(&mut self, cx: &mut Context<Self>) {
        self.keys_restore = KeysRestore::Asking;
        cx.notify();
    }

    /// A tap on the confirm row's Cancel: nothing changes.
    fn cancel_keys_restore(&mut self, cx: &mut Context<Self>) {
        self.keys_restore = KeysRestore::Idle;
        cx.notify();
    }

    /// A tap on the confirm row's Restore: the grid goes back to its built-in
    /// keys.
    fn restore_keys_defaults(&mut self, cx: &mut Context<Self>) {
        self.keys_restore = KeysRestore::Idle;
        self.root
            .update(cx, |root, cx| root.set_phone_keys(None, cx));
        cx.notify();
    }

    /// The Keys page: the hint, the grid in as many equal columns as the
    /// window fits, then Restore defaults.
    pub(super) fn keys_page(
        &self,
        window: &mut Window,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        let saved = self.saved_keys(cx);
        let keys = key_grid::resolved(saved.as_deref());
        let width = f32::from(window.viewport_size().width) - 2.0 * SIDE_PADDING;
        let columns = key_grid::grid_columns(width, CELL_MIN, CELL_GAP);
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .child(
                div()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .text_color(gpui::rgb(t.muted))
                    .child(HINT),
            )
            .child(Self::keys_grid(&keys, columns, cx))
            .child(self.keys_restore_row(t, cx))
    }

    /// The grid: `columns` equal cells a row, the last row's empty places
    /// kept so every cell is the same width.
    fn keys_grid(keys: &[GridKey], columns: usize, cx: &mut Context<Self>) -> Div {
        let mut grid = div()
            .driver_id(|| "phone-keys-grid".to_owned())
            .flex()
            .flex_col()
            .gap(px(CELL_GAP));
        for (row, chunk) in keys.chunks(columns).enumerate() {
            let mut line = div().flex().gap(px(CELL_GAP));
            for (offset, key) in chunk.iter().enumerate() {
                let index = row * columns + offset;
                line = line.child(Self::key_cell(index, key, cx));
            }
            for _ in chunk.len()..columns {
                line = line.child(div().flex_1());
            }
            grid = grid.child(line);
        }
        grid
    }

    /// One cell: the key's label, dimmed while the key does nothing.
    fn key_cell(index: usize, key: &GridKey, cx: &mut Context<Self>) -> Stateful<Div> {
        let dimmed = key_grid::is_inert(key);
        phone_button(
            &format!("phone-keys-cell-{index}"),
            ButtonKind::Outlined,
            cx,
        )
        .flex_1()
        .min_w_0()
        .min_h(px(TOUCH_TARGET))
        .child(key.label.clone())
        .when(dimmed, |cell| {
            cell.opacity(crate::spawn_view::DISABLED_OPACITY)
        })
    }

    /// Restore defaults: the button, or the row that asks first. It is dimmed
    /// and takes no tap while the built-in grid already shows.
    fn keys_restore_row(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        if self.keys_restore == KeysRestore::Asking {
            return Self::keys_restore_confirm_row(t, cx);
        }
        let saved = self.saved_keys(cx).is_some();
        let button = restore_button("phone-keys-restore", saved, RESTORE_ROW_LABEL, t, cx);
        let button = if saved {
            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.ask_keys_restore(cx)))
        } else {
            button
        };
        div().flex().items_center().child(button)
    }

    /// The question Restore defaults turns into, with its Restore and Cancel.
    fn keys_restore_confirm_row(t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let confirm = restore_button("phone-keys-restore-confirm", true, RESTORE_LABEL, t, cx)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.restore_keys_defaults(cx);
            }));
        let cancel = restore_button("phone-keys-restore-cancel", true, CANCEL_LABEL, t, cx)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_keys_restore(cx)));
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(div().flex_1().min_w_0().child(RESTORE_ASK))
            .child(confirm)
            .child(cancel)
    }
}
