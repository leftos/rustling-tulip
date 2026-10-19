//! The three "section" shapes the client draws, named for their roles. A
//! Settings page heading, a form-field column and a flyout heading share the
//! old name but not one look; each keeps the pixels it had.

use gpui::{Div, FontWeight, div, prelude::*, px};

use crate::spawn_view::field_label;
use crate::{BORDER, MUTED};

/// The size of a Settings page's section head.
const SECTION_HEAD_SIZE: f32 = 14.0;

/// A Settings page's group of rows: a bold heading, the rows under it.
pub(crate) fn section(title: &'static str) -> Div {
    div().flex().flex_col().gap(px(10.0)).pb(px(18.0)).child(
        div()
            .text_size(px(SECTION_HEAD_SIZE))
            .font_weight(FontWeight::BOLD)
            .child(title),
    )
}

/// A form-field column: its label, then what the caller adds under it.
pub(crate) fn field_column(label: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(7.0))
        .child(field_label(label))
}

/// A flyout section: a top border and a muted heading.
pub(crate) fn flyout_section(label: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .pt(px(6.0))
        .border_t_1()
        .border_color(gpui::rgb(BORDER))
        .child(div().text_color(gpui::rgb(MUTED)).child(label))
}
