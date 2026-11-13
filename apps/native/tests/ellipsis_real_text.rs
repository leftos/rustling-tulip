//! A line that ends in `…`, measured narrow and then wide, on the
//! platform's real text system. gpui's test platform shapes with a no-op text
//! system that never slices the text by its runs, so a UI spec cannot see this
//! fault; DirectWrite can, and panics.
//!
//! The three widths are the chain the Dashboard's By status view measures its
//! older summary line at on a 2560 px window: nothing, room for one character
//! and the `…`, then the whole column.
#![cfg(windows)]

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, AvailableSpace, Bounds, Context, Element, ElementId,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, Render, SharedString,
    Style, StyledText, Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use rustling_tulip_native::offscreen::show_cloaked;

/// The line the owner's Dashboard panicked on: `·` starts at byte 3, so a run
/// of 4 bytes ends inside it.
const OLDER_LINE: &str = "1m · The inbox has 0 unread.";
const WHOLE_COLUMN: f32 = 505.0;

/// Lays its one line out three times over, then draws nothing.
struct ThreeWidths {
    text: &'static str,
    measured: Rc<Cell<bool>>,
}

impl IntoElement for ThreeWidths {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for ThreeWidths {
    type RequestLayoutState = (AnyElement, LayoutId);
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // What `ellipsis::truncating` builds.
        let mut line = div()
            .min_w(px(0.0))
            .overflow_hidden()
            .text_ellipsis()
            .line_clamp(1)
            .child(StyledText::new(self.text))
            .into_any_element();
        let line_id = line.request_layout(window, cx);
        let root = window.request_layout(Style::default(), [line_id], cx);
        (root, (line, line_id))
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (_, line_id): &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let one_char_and_ellipsis = width_of("1", window) + width_of("…", window) + 1.0;
        for width in [0.0, one_char_and_ellipsis, WHOLE_COLUMN] {
            window.compute_layout(
                *line_id,
                size(
                    AvailableSpace::Definite(px(width)),
                    AvailableSpace::MinContent,
                ),
                cx,
            );
        }
        self.measured.set(true);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        (): &mut Self::PrepaintState,
        _: &mut Window,
        _: &mut App,
    ) {
    }
}

/// `text`'s width in px in the window's current text style.
fn width_of(text: &str, window: &Window) -> f32 {
    let style = window.text_style();
    let run = style.to_run(text.len());
    let size = style.font_size.to_pixels(window.rem_size());
    let line =
        window
            .text_system()
            .shape_line(SharedString::from(text.to_owned()), size, &[run], None);
    line.width / px(1.0)
}

struct Host {
    text: &'static str,
    measured: Rc<Cell<bool>>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        ThreeWidths {
            text: self.text,
            measured: self.measured.clone(),
        }
    }
}

/// Opens a cloaked window over `text`, lays it out at the three widths, and
/// quits; a shaping that slices `text` inside a character panics first.
fn lay_out_three_times(text: &'static str) {
    let measured = Rc::new(Cell::new(false));
    let seen = measured.clone();
    let done = measured.clone();
    gpui_platform::application().run(move |cx: &mut App| {
        let window_size = (800, 200);
        let bounds = Bounds::centered(None, size(px(800.0), px(200.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                focus: false,
                show: false,
                ..Default::default()
            },
            move |window, cx| {
                show_cloaked(window, window_size);
                cx.new(|_| Host { text, measured })
            },
        );
        assert!(opened.is_ok(), "opening the window: {:?}", opened.err());
        cx.spawn(async move |cx| {
            while !done.get() {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(20))
                    .await;
            }
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });
    assert!(seen.get(), "the window never laid the line out");
}

#[test]
#[ignore = "opens a window: run through rt.ps1 native-smoke"]
fn a_line_measured_narrow_then_wide_shapes_on_a_char_boundary() {
    lay_out_three_times(OLDER_LINE);
}

/// The same three widths over a line whose stale run still ends on a
/// character: the rig itself lays out and quits cleanly.
#[test]
#[ignore = "opens a window: run through rt.ps1 native-smoke"]
fn control_a_line_whose_short_run_ends_on_a_char_boundary() {
    lay_out_three_times("15m \u{b7} The inbox has 0 unread.");
}

/// A Greek session label: the run a 0 px measure leaves ends inside `ο`.
#[test]
#[ignore = "opens a window: run through rt.ps1 native-smoke"]
fn a_label_starting_with_a_two_byte_character_shapes_on_a_char_boundary() {
    lay_out_three_times("Δοκιμή");
}
