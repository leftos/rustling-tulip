//! One line of text that ends in `…` when its box is too narrow for it.
//!
//! gpui's own `.truncate()` (`overflow_hidden`, `whitespace_nowrap`,
//! `text_ellipsis`) never draws the `…` in gpui 0.2.2: gpui returns a text
//! layout's cached size whenever a measure call carries no wrap width
//! (`elements/text.rs`), and a nowrap text's first measure is the indefinite
//! one, so the later measure at the box's real width never truncates. The
//! two helpers here each meet a definite width another way:
//!
//! - [`ellipsized`] for a box sized by its own content that may shrink (a
//!   title among chips, a name capped by `max_w`): an invisible copy sizes
//!   the box and an absolutely positioned copy draws over it.
//! - [`truncating`] for a box whose width its parent sets (`flex_1`, a block
//!   row): wrapping stays on, so gpui measures again at the narrowed width.
//!
//! In debug builds, once a test has called [`enable_probe`], each helper
//! records the text it drew under its id, and [`drawn_text`] reads it back,
//! so specs can see the `…`. A normal run never records.

#[cfg(debug_assertions)]
use std::cell::{Cell, RefCell};
#[cfg(debug_assertions)]
use std::collections::HashMap;

#[cfg(debug_assertions)]
use gpui::TextLayout;
use gpui::{AnyElement, Div, SharedString, StyledText, div, prelude::*, px};

#[cfg(debug_assertions)]
thread_local! {
    /// The layout each helper last drew, by the id it was given.
    static DRAWN: RefCell<HashMap<String, TextLayout>> = RefCell::new(HashMap::new());
    /// Whether this thread records into [`DRAWN`]: off until a test
    /// switches it on.
    static PROBING: Cell<bool> = const { Cell::new(false) };
}

/// Switches the probe on for this thread, so the helpers record what they
/// draw for [`drawn_text`]. Specs call it first; nothing else does.
#[cfg(debug_assertions)]
pub fn enable_probe() {
    PROBING.with(|probing| probing.set(true));
}

/// Notes `drawn`'s layout under `id`, for [`drawn_text`], while the probe is
/// on. Release builds keep nothing.
fn record(id: &str, drawn: &StyledText) {
    #[cfg(debug_assertions)]
    if PROBING.with(Cell::get) {
        DRAWN.with(|layouts| {
            layouts
                .borrow_mut()
                .insert(id.to_owned(), drawn.layout().clone());
        });
    }
    #[cfg(not(debug_assertions))]
    let _ = (id, drawn);
}

/// The text the element drawn under `id` last showed, `…` included, or
/// `None` when no helper has drawn under that id on this thread since
/// [`enable_probe`]. The element
/// must have been laid out since it was built, as gpui's layout panics
/// before its first measure.
#[cfg(debug_assertions)]
#[must_use]
pub fn drawn_text(id: &str) -> Option<String> {
    DRAWN.with(|layouts| layouts.borrow().get(id).map(TextLayout::text))
}

/// A title as a pair, for a box sized by its content: an invisible copy that
/// sizes the box to the whole text, and a drawn copy laid over it that ends
/// in `…` when the box is too narrow. The caller's box is
/// `relative().min_w(0).overflow_hidden().whitespace_nowrap()` (or caps its
/// width another way); the absolutely positioned copy meets the box's
/// definite width straight away. `id` names the drawn copy for
/// [`drawn_text`].
pub(crate) fn ellipsized(id: &str, text: SharedString, drawn: StyledText) -> [AnyElement; 2] {
    record(id, &drawn);
    [
        div().invisible().child(text).into_any_element(),
        div()
            .absolute()
            .inset_0()
            .text_ellipsis()
            .child(drawn)
            .into_any_element(),
    ]
}

/// One line of text that ends in `…` when its parent sets its width
/// narrower than the text: a `flex_1` or shrinking flex item, or a block
/// row. Wrapping stays on so gpui re-measures the text at the narrowed
/// width, and the one-line clamp keeps it to a line. `id` names the text
/// for [`drawn_text`].
pub(crate) fn truncating(id: &str, text: StyledText) -> Div {
    record(id, &text);
    div()
        .min_w(px(0.0))
        .overflow_hidden()
        .text_ellipsis()
        .line_clamp(1)
        .child(text)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use gpui::{
        Context, IntoElement, ParentElement, Render, SharedString, Styled, StyledText,
        TestAppContext, Window, div, px,
    };

    use super::{drawn_text, ellipsized, enable_probe, truncating};

    const TITLE: &str = "row-title";
    const LINE: &str = "row-line";

    /// A 120 px row: the text in the shape under test, then a 40 px
    /// sibling that never shrinks.
    struct Row {
        text: SharedString,
        flexible: bool,
    }

    impl Render for Row {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let label = StyledText::new(self.text.clone());
            let text = if self.flexible {
                truncating(LINE, label).flex_1()
            } else {
                div()
                    .relative()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .children(ellipsized(TITLE, self.text.clone(), label))
            };
            div()
                .flex()
                .w(px(120.0))
                .child(text)
                .child(div().flex_none().w(px(40.0)).h(px(20.0)))
        }
    }

    /// Draws `text` in a row, with the probe on, and hands back what the
    /// shape drew.
    fn drawn(text: &'static str, flexible: bool, cx: &mut TestAppContext) -> Option<String> {
        enable_probe();
        drawn_unprobed(text, flexible, cx)
    }

    /// Draws `text` in a row and hands back what the probe holds for it.
    fn drawn_unprobed(
        text: &'static str,
        flexible: bool,
        cx: &mut TestAppContext,
    ) -> Option<String> {
        let row = Row {
            text: text.into(),
            flexible,
        };
        let (_view, cx) = cx.add_window_view(|_, _| row);
        cx.run_until_parked();
        drawn_text(if flexible { LINE } else { TITLE })
    }

    #[gpui::test]
    fn the_probe_records_only_once_switched_on(cx: &mut TestAppContext) {
        assert_eq!(
            drawn_unprobed("Fix", true, cx),
            None,
            "a run that never switched the probe on records nothing"
        );
        assert_eq!(drawn("Fix", true, cx).as_deref(), Some("Fix"));
    }

    #[gpui::test]
    fn a_title_too_long_for_its_box_ends_in_an_ellipsis(cx: &mut TestAppContext) {
        let text = drawn("Tighten the footer pill spacing", false, cx);
        assert!(
            text.as_deref().is_some_and(|text| text.ends_with('…')),
            "the drawn title is {text:?}"
        );
    }

    #[gpui::test]
    fn a_title_that_fits_is_drawn_whole(cx: &mut TestAppContext) {
        let text = drawn("Fix", false, cx);
        assert_eq!(
            text.as_deref(),
            Some("Fix"),
            "a title that fits is drawn whole"
        );
    }

    #[gpui::test]
    fn a_line_too_long_for_its_row_ends_in_an_ellipsis(cx: &mut TestAppContext) {
        let text = drawn("Tighten the footer pill spacing", true, cx);
        assert!(
            text.as_deref().is_some_and(|text| text.ends_with('…')),
            "the drawn line is {text:?}"
        );
    }

    #[gpui::test]
    fn a_line_that_fits_is_drawn_whole(cx: &mut TestAppContext) {
        let text = drawn("Fix", true, cx);
        assert_eq!(
            text.as_deref(),
            Some("Fix"),
            "a line that fits is drawn whole"
        );
    }
}
