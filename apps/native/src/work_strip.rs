//! The running-work strip drawn as one row: the parts of a [`Strip`] side
//! by side, each part a segment whose tooltip lists its items' tooltips, a
//! lone shell's command in the terminal font. Nothing on it takes a click.
//! It knows nothing of the pane that draws it.

use gpui::{AnyElement, ElementId, IntoElement, SharedString, div, prelude::*, px};

use crate::chat::{STRIP_SEPARATOR, Strip, StripItem, StripKind};
use crate::chat_view::{BODY_PAD_X, SMALL_TEXT_SIZE};
use crate::fonts::ui_px_at;
use crate::tooltip;
use crate::ui_theme::UiTheme;

/// The strip's height, which holds one line of text.
const STRIP_HEIGHT: f32 = 28.0;

/// `strip` as a row at UI text size `offset`, with the shell command drawn
/// in `mono`.
pub fn strip_row(
    offset: i8,
    strip: &Strip,
    mono: &SharedString,
    t: &UiTheme,
) -> impl IntoElement + use<> {
    let mut children: Vec<AnyElement> = Vec::with_capacity(strip.parts.len() * 2);
    for (index, part) in strip.parts.iter().enumerate() {
        if index > 0 {
            children.push(
                div()
                    .text_color(gpui::rgb(t.faint))
                    .child(STRIP_SEPARATOR)
                    .into_any_element(),
            );
        }
        let items: Vec<&StripItem> = strip
            .items
            .iter()
            .filter(|item| item.part == index)
            .collect();
        children.push(segment(index, part, &items, mono));
    }
    div()
        .id("work-strip")
        .debug_selector(|| "work-strip".to_owned())
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .whitespace_nowrap()
        .overflow_hidden()
        .px(px(BODY_PAD_X))
        .h(ui_px_at(offset, STRIP_HEIGHT))
        .border_t_1()
        .border_color(gpui::rgb(t.line))
        .text_size(ui_px_at(offset, SMALL_TEXT_SIZE))
        .text_color(gpui::rgb(t.text_2))
        .children(children)
}

/// Part `index` of the strip, `text`, counting `items`: its tooltip lists
/// theirs, one per line, and it has none when none of them has one.
fn segment(index: usize, text: &str, items: &[&StripItem], mono: &SharedString) -> AnyElement {
    let name = format!("work-item-{index}");
    let tip = segment_tip(items);
    let segment = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .flex_row()
        .children(segment_text(text, items, mono));
    match tip {
        Some(tip) => segment.tooltip(tooltip(tip)).into_any_element(),
        None => segment.into_any_element(),
    }
}

/// A segment's tooltip: its items' tooltips one per line, each said once,
/// in order; `None` when none has one.
fn segment_tip(items: &[&StripItem]) -> Option<String> {
    let mut tips: Vec<&str> = Vec::new();
    for tip in items.iter().filter_map(|item| item.tooltip.as_deref()) {
        if !tips.contains(&tip) {
            tips.push(tip);
        }
    }
    (!tips.is_empty()).then(|| tips.join("\n"))
}

/// A part's text as drawn: a lone shell's command (its item's label) in
/// `mono`, the rest as it is.
fn segment_text(text: &str, items: &[&StripItem], mono: &SharedString) -> Vec<AnyElement> {
    let plain = |text: &str| div().child(text.to_owned()).into_any_element();
    let [item] = items else {
        return vec![plain(text)];
    };
    let marker = format!("({}, ", item.label);
    if item.kind != StripKind::Shell {
        return vec![plain(text)];
    }
    let Some((before, after)) = text.split_once(&marker) else {
        return vec![plain(text)];
    };
    vec![
        plain(&format!("{before}(")),
        div()
            .font_family(mono.clone())
            .font_weight(crate::fonts::mono_weight())
            .child(item.label.clone())
            .into_any_element(),
        plain(&format!(", {after}")),
    ]
}

#[cfg(test)]
mod tests {
    use crate::chat::{SCREEN_READ_TIP, StripItem, StripKind};

    use super::segment_tip;

    fn shell_item(tooltip: Option<&str>) -> StripItem {
        StripItem {
            kind: StripKind::Shell,
            label: "background shell".to_owned(),
            tooltip: tooltip.map(str::to_owned),
            tappable: false,
            part: 0,
        }
    }

    #[test]
    fn two_screen_read_shells_say_their_tooltip_once() {
        let first = shell_item(Some(SCREEN_READ_TIP));
        let second = shell_item(Some(SCREEN_READ_TIP));
        assert_eq!(
            segment_tip(&[&first, &second]).as_deref(),
            Some(SCREEN_READ_TIP)
        );
    }

    #[test]
    fn distinct_tooltips_keep_their_order_one_per_line() {
        let items = [
            shell_item(Some("cargo test")),
            shell_item(None),
            shell_item(Some(SCREEN_READ_TIP)),
            shell_item(Some("cargo test")),
        ];
        let refs: Vec<&StripItem> = items.iter().collect();
        assert_eq!(
            segment_tip(&refs),
            Some(format!("cargo test\n{SCREEN_READ_TIP}"))
        );
        assert_eq!(segment_tip(&[&shell_item(None)]), None);
    }
}
