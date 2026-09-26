//! Diff view specs: the side-by-side view on a window of its own, its rows
//! and line numbers, the change keys, and sideways scrolling.

use std::time::{Duration, Instant};

use gpui::{
    Entity, Modifiers, Pixels, Point, ScrollDelta, ScrollStrategy, ScrollWheelEvent,
    TestAppContext, VisualTestContext, point,
};
use rustling_tulip_native::diff_model::{DiffModel, DiffOptions};
use rustling_tulip_native::diff_view::{DiffView, Nav};
use rustling_tulip_native::fonts::{self, FontSettings};

/// The view of `old` against `new` on a window of its own, focused.
fn mount<'a>(
    cx: &'a mut TestAppContext,
    old: &str,
    new: &str,
) -> (Entity<DiffView>, &'a mut VisualTestContext) {
    cx.update(|cx| fonts::register_bundled(cx));
    let model = DiffModel::build(old, new, DiffOptions::default());
    let (view, cx) = cx.add_window_view(|_, cx| DiffView::new(model, FontSettings::default(), cx));
    let focus = cx.update(|_, cx| view.read(cx).focus_handle().clone());
    cx.update(|window, _| focus.focus(window));
    cx.run_until_parked();
    (view, cx)
}

fn read<R>(
    cx: &mut VisualTestContext,
    view: &Entity<DiffView>,
    f: impl FnOnce(&DiffView) -> R,
) -> R {
    cx.update(|_, cx| f(view.read(cx)))
}

fn go(cx: &mut VisualTestContext, view: &Entity<DiffView>, nav: Nav) {
    cx.update(|_, cx| view.update(cx, |view, cx| view.go(nav, cx)));
    cx.run_until_parked();
}

fn window_center(cx: &mut VisualTestContext) -> Point<Pixels> {
    let size = cx.update(|window, _| window.viewport_size());
    point(size.width / 2.0, size.height / 2.0)
}

fn wheel(cx: &mut VisualTestContext, at: Point<Pixels>, lines: f32, shift: bool) {
    cx.simulate_event(ScrollWheelEvent {
        position: at,
        delta: ScrollDelta::Lines(point(0.0, lines)),
        modifiers: Modifiers {
            shift,
            ..Modifiers::none()
        },
        ..ScrollWheelEvent::default()
    });
    cx.run_until_parked();
}

/// A wheel turned sideways by `columns`, the way Windows delivers Shift+wheel
/// (gpui turns it into a horizontal-only delta).
fn wheel_sideways(cx: &mut VisualTestContext, at: Point<Pixels>, columns: f32, shift: bool) {
    cx.simulate_event(ScrollWheelEvent {
        position: at,
        delta: ScrollDelta::Lines(point(columns, 0.0)),
        modifiers: Modifiers {
            shift,
            ..Modifiers::none()
        },
        ..ScrollWheelEvent::default()
    });
    cx.run_until_parked();
}

/// The new side's shown text in drawn row `row`.
fn shown_new(cx: &mut VisualTestContext, view: &Entity<DiffView>, row: usize) -> String {
    read(cx, view, |v| {
        v.shown_text()
            .get(row)
            .and_then(|(_, new)| new.as_ref())
            .map(ToString::to_string)
            .unwrap_or_default()
    })
}

/// `lines` numbered lines, with the lines at `changed` (0-based) edited on
/// the new side.
fn numbered(lines: usize, changed: &[usize]) -> (String, String) {
    let old: Vec<String> = (0..lines).map(|i| format!("line {i}")).collect();
    let new: Vec<String> = (0..lines)
        .map(|i| {
            if changed.contains(&i) {
                format!("line {i} edited")
            } else {
                format!("line {i}")
            }
        })
        .collect();
    (join(&old), join(&new))
}

#[gpui::test]
fn a_small_diff_shows_every_row_with_both_sides_line_numbers(cx: &mut TestAppContext) {
    let (view, cx) = mount(cx, "a\nb\nc\nd\n", "a\nB\nc\nd\ne\n");
    assert_eq!(read(cx, &view, |v| v.model().rows().len()), 5);
    assert_eq!(read(cx, &view, DiffView::rendered_rows), 0..5);
    assert_eq!(
        read(cx, &view, DiffView::shown_line_numbers),
        [
            (Some(1), Some(1)),
            (Some(2), Some(2)),
            (Some(3), Some(3)),
            (Some(4), Some(4)),
            (None, Some(5)),
        ]
    );
}

#[gpui::test]
fn f7_and_shift_f7_walk_the_hunks_and_wrap(cx: &mut TestAppContext) {
    let (view, cx) = mount(cx, "a\nb\nc\nd\ne\nf\ng\n", "a\nB\nc\nd\nE\nf\nG\n");
    assert_eq!(read(cx, &view, DiffView::current_hunk), None);
    let mut walked = Vec::new();
    for _ in 0..4 {
        cx.simulate_keystrokes("f7");
        walked.push(read(cx, &view, DiffView::current_hunk));
    }
    assert_eq!(
        walked,
        [Some(0), Some(1), Some(2), Some(0)],
        "F7 wraps to the first"
    );
    cx.simulate_keystrokes("shift-f7");
    assert_eq!(
        read(cx, &view, DiffView::current_hunk),
        Some(2),
        "Shift+F7 wraps to the last"
    );
    cx.simulate_keystrokes("shift-f7");
    assert_eq!(read(cx, &view, DiffView::current_hunk), Some(1));
}

#[gpui::test]
fn going_to_the_last_hunk_scrolls_it_into_view(cx: &mut TestAppContext) {
    let (old, new) = numbered(600, &[5, 300, 590]);
    let (view, cx) = mount(cx, &old, &new);
    assert!(
        !read(cx, &view, DiffView::rendered_rows).contains(&590),
        "the last hunk starts off screen"
    );
    go(cx, &view, Nav::Last);
    assert_eq!(read(cx, &view, DiffView::current_hunk), Some(2));
    assert!(
        read(cx, &view, DiffView::rendered_rows).contains(&590),
        "rows drawn: {:?}",
        read(cx, &view, DiffView::rendered_rows)
    );
    assert!(read(cx, &view, DiffView::scroll_top) > 0.0);
    go(cx, &view, Nav::First);
    assert!(read(cx, &view, DiffView::rendered_rows).contains(&5));
}

#[gpui::test]
fn shift_wheel_scrolls_sideways_and_leaves_the_rows_where_they_are(cx: &mut TestAppContext) {
    let long = format!("start {} end", "x".repeat(400));
    let (old, new) = numbered(300, &[1]);
    let old = format!("{old}{long}\n");
    let new = format!("{new}{long} changed\n");
    let (view, cx) = mount(cx, &old, &new);
    let at = window_center(cx);

    wheel(cx, at, -3.0, false);
    let top = read(cx, &view, DiffView::scroll_top);
    assert!(top > 0.0, "a plain wheel scrolls the rows");
    assert!(read(cx, &view, DiffView::h_offset).abs() < f32::EPSILON);

    wheel(cx, at, -3.0, true);
    let sideways = read(cx, &view, DiffView::h_offset);
    assert!(
        sideways > 0.0,
        "Shift+wheel scrolls the lines right: top {top} -> {}",
        read(cx, &view, DiffView::scroll_top)
    );
    assert!(
        (read(cx, &view, DiffView::scroll_top) - top).abs() < f32::EPSILON,
        "and leaves the rows where they are"
    );

    wheel(cx, at, 30.0, true);
    assert!(
        read(cx, &view, DiffView::h_offset).abs() < f32::EPSILON,
        "scrolling back stops at the left edge"
    );
}

#[gpui::test]
fn windows_shift_wheel_arrives_sideways_and_scrolls_sideways(cx: &mut TestAppContext) {
    let long = format!("start {} end", "x".repeat(400));
    let (old, new) = numbered(300, &[1]);
    let (view, cx) = mount(cx, &format!("{old}{long}\n"), &format!("{new}{long}!\n"));
    let at = window_center(cx);
    wheel_sideways(cx, at, -3.0, true);
    assert!(
        read(cx, &view, DiffView::h_offset) > 0.0,
        "Shift+wheel on Windows scrolls right"
    );
    assert!(
        read(cx, &view, DiffView::scroll_top).abs() < f32::EPSILON,
        "and leaves the rows where they are"
    );
}

#[gpui::test]
fn a_multi_byte_word_change_draws_whole_lines(cx: &mut TestAppContext) {
    let (view, cx) = mount(cx, "naïve café\n", "naïve cafés\n");
    let spans = read(cx, &view, |v| v.model().inline(0).cloned());
    let spans = spans.unwrap_or_default();
    assert_eq!(spans.right.len(), 1, "one changed word: {:?}", spans.right);
    assert_eq!(
        spans.right.first(),
        Some(&(7..13)),
        "cafés, on char boundaries"
    );
    assert_eq!(shown_new(cx, &view, 0), "naïve cafés");
}

#[gpui::test]
fn a_long_line_scrolled_right_shows_its_tail(cx: &mut TestAppContext) {
    let line = format!("HEAD{}TAIL", "abcdefghij".repeat(1000));
    let text = format!("{line}\n");
    let (view, cx) = mount(cx, &text, &text);
    let head = shown_new(cx, &view, 0);
    assert!(head.starts_with("HEAD"), "unscrolled, the head shows");
    assert!(
        head.chars().count() < 1000,
        "and only what fits: {} chars",
        head.chars().count()
    );
    let at = window_center(cx);
    wheel_sideways(cx, at, -100_000.0, true);
    let tail = shown_new(cx, &view, 0);
    assert!(
        tail.ends_with("TAIL"),
        "scrolled to the end, the tail shows"
    );
    assert!(!tail.contains("HEAD"), "not the head");
    assert!(
        tail.chars().count() < 1000,
        "and only what fits: {} chars",
        tail.chars().count()
    );
}

#[gpui::test]
fn a_diff_of_short_lines_does_not_scroll_sideways(cx: &mut TestAppContext) {
    let (view, cx) = mount(cx, "0123456789\n", "012345678X\n");
    let at = window_center(cx);
    wheel(cx, at, -3.0, true);
    wheel_sideways(cx, at, -3.0, false);
    assert!(
        read(cx, &view, DiffView::h_offset).abs() < f32::EPSILON,
        "a 10-char line fits its column: {}",
        read(cx, &view, DiffView::h_offset)
    );
}

#[gpui::test]
fn scrolling_right_stops_with_the_longest_lines_last_character_in_view(cx: &mut TestAppContext) {
    let line = format!("{}Z", "x".repeat(199));
    let text = format!("{line}\n");
    let (view, cx) = mount(cx, &text, &text);
    let at = window_center(cx);
    wheel_sideways(cx, at, -100_000.0, true);
    let stopped = read(cx, &view, DiffView::h_offset);
    let columns = read(cx, &view, DiffView::visible_columns);
    assert!(columns.start > 0, "scrolled right: {columns:?}");
    assert!(
        (200..=201).contains(&columns.end),
        "the last character is the last one in view: {columns:?}"
    );
    assert!(shown_new(cx, &view, 0).ends_with('Z'));
    wheel_sideways(cx, at, -3.0, true);
    assert!(
        (read(cx, &view, DiffView::h_offset) - stopped).abs() < f32::EPSILON,
        "it scrolls no further"
    );
}

#[gpui::test]
fn widening_the_window_keeps_the_last_character_at_the_right_edge(cx: &mut TestAppContext) {
    let line = format!("{}Z", "x".repeat(199));
    let text = format!("{line}\n");
    let (view, cx) = mount(cx, &text, &text);
    cx.simulate_resize(gpui::size(gpui::px(600.0), gpui::px(400.0)));
    cx.run_until_parked();
    let at = window_center(cx);
    wheel_sideways(cx, at, -100_000.0, true);
    let narrow = read(cx, &view, DiffView::h_offset);
    assert!(narrow > 0.0, "scrolled right");

    cx.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(400.0)));
    cx.run_until_parked();
    let wide = read(cx, &view, DiffView::h_offset);
    let columns = read(cx, &view, DiffView::visible_columns);
    assert!(
        wide < narrow,
        "a wider column scrolls less far: {narrow} -> {wide}"
    );
    assert!(
        (200..=201).contains(&columns.end),
        "the last character sits at the right edge: {columns:?}"
    );
}

#[gpui::test]
fn f7_goes_to_the_next_change_below_the_drawn_rows(cx: &mut TestAppContext) {
    let (old, new) = numbered(600, &[5, 590]);
    let (view, cx) = mount(cx, &old, &new);
    let handle = cx.update(|_, cx| view.read(cx).scroll_handle().clone());
    handle.scroll_to_item_strict(580, ScrollStrategy::Top);
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(
        !read(cx, &view, DiffView::rendered_rows).contains(&5),
        "the first hunk is off screen"
    );
    cx.simulate_keystrokes("f7");
    assert_eq!(
        read(cx, &view, DiffView::current_hunk),
        Some(1),
        "F7 goes to the change at or below the drawn rows, not the first"
    );
}

/// A Rust-like source of `lines` lines.
fn rust_like(lines: usize) -> Vec<String> {
    (0..lines)
        .map(|i| match i % 6 {
            0 => format!("fn item_{i}(value: u32) -> u32 {{"),
            1 => format!("    let total = value * {i} + {};", i % 13),
            2 => format!("    // step {i}: fold the running total"),
            3 => format!("    let next = total.wrapping_add({});", i % 7),
            4 => "    next".to_owned(),
            _ => "}".to_owned(),
        })
        .collect()
}

/// `base` with `hunks` evenly spaced changes: three lines edited in each,
/// and an inserted or a deleted line in every other.
fn scattered_changes(base: &[String], hunks: usize) -> Vec<String> {
    let stride = base.len() / hunks;
    let mut out = Vec::with_capacity(base.len() + hunks);
    for (i, line) in base.iter().enumerate() {
        let hunk = i / stride;
        let at = if hunk < hunks { i % stride } else { 0 };
        match at {
            10..=12 => out.push(format!("{line} // edited")),
            13 if hunk.is_multiple_of(2) => {
                out.push(format!("    // inserted in hunk {hunk}"));
                out.push(line.clone());
            }
            13 => {}
            _ => out.push(line.clone()),
        }
    }
    out
}

fn join(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// One full draw of the window.
fn time_draw(cx: &mut VisualTestContext) -> Duration {
    cx.update(|window, cx| {
        let started = Instant::now();
        window.draw(cx).clear();
        started.elapsed()
    })
}

#[gpui::test]
#[ignore = "spike timing: run with -- --ignored --nocapture"]
#[expect(clippy::print_stderr, reason = "spike timing output")]
fn spike_draw_timings(cx: &mut TestAppContext) {
    cx.update(|cx| fonts::register_bundled(cx));
    let base = rust_like(5_000);
    let old = join(&base);
    let scattered = join(&scattered_changes(&base, 60));
    let all: Vec<String> = base.iter().map(|line| format!("{line} // all")).collect();
    let all = join(&all);
    for (name, new) in [("5k, 60 hunks", &scattered), ("5k, every line", &all)] {
        let started = Instant::now();
        let model = DiffModel::build(&old, new, DiffOptions::default());
        let build = started.elapsed();
        let started = Instant::now();
        let (view, cx) =
            cx.add_window_view(|_, cx| DiffView::new(model, FontSettings::default(), cx));
        let mount = started.elapsed();
        let redraw = time_draw(cx);
        let handle = cx.update(|_, cx| view.read(cx).scroll_handle().clone());
        let mut steps = Vec::with_capacity(50);
        for step in 1..=50 {
            handle.scroll_to_item_strict(step * 40, ScrollStrategy::Top);
            steps.push(time_draw(cx));
        }
        let rows = read(cx, &view, DiffView::rendered_rows);
        let total: Duration = steps.iter().sum();
        let worst = steps.iter().max().copied().unwrap_or_default();
        eprintln!(
            "{name}: build {build:?}; mount (window + first draw) {mount:?}; full redraw {redraw:?}; \
             50 scroll steps of 40 rows: total {total:?}, mean {:?}, worst {worst:?}; \
             rows drawn per frame {}",
            total / 50,
            rows.len(),
        );
    }
}
