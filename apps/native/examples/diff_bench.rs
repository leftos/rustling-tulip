//! Times the side-by-side diff view in a real window, then quits: the model
//! build over a synthetic 5,000-line pair (about a tenth of the lines
//! changed, a few of them 2,000 characters long), the time from opening the
//! window to its first drawn rows, and the worst of 200 scroll steps driven
//! through the list's scroll handle. It talks to no daemon.
//!
//! With `RUSTLING_TULIP_OFFSCREEN_WINDOW` set to a non-empty value the window
//! opens cloaked and is never activated, as the client's does.
//!
//! `cargo run --release -p rustling-tulip-native --example diff_bench`

use std::time::{Duration, Instant};

use gpui::{
    App, Application, Bounds, Entity, ScrollStrategy, Window, WindowBounds, WindowOptions,
    prelude::*, px, size,
};
use rustling_tulip_native::diff_model::{DiffModel, DiffOptions};
use rustling_tulip_native::diff_view::DiffView;
use rustling_tulip_native::fonts::{self, FontSettings};
use rustling_tulip_native::offscreen::{requested, show_cloaked};

/// The window's size in logical pixels, the client's own.
const WINDOW_SIZE: (u16, u16) = (1000, 640);
const LINES: usize = 5_000;
const STEPS: usize = 200;
/// How many rows each scroll step moves down.
const STEP_ROWS: usize = 25;

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    Application::new().run(|cx: &mut App| {
        fonts::register_bundled(cx);
        let (old, new) = synthetic_pair();
        let started = Instant::now();
        let model = DiffModel::build(&old, &new, DiffOptions::default());
        let build = started.elapsed();
        tracing::info!(
            rows = model.rows().len(),
            hunks = model.change_count(),
            ?build,
            "model built"
        );
        open(model, cx);
    });
}

/// A Rust-like 5,000-line text and a copy with every tenth line edited; the
/// lines at 503, 1503, … are 2,000-character literals, edited too.
fn synthetic_pair() -> (String, String) {
    let (mut old, mut new) = (String::new(), String::new());
    for i in 0..LINES {
        let line = if i % 1000 == 503 {
            format!("    let blob = \"{}\";", "0123456789abcdef".repeat(125))
        } else {
            match i % 6 {
                0 => format!("fn item_{i}(value: u32) -> u32 {{"),
                1 => format!("    let total = value * {i} + {};", i % 13),
                2 => format!("    // step {i}: fold the running total"),
                3 => format!("    let next = total.wrapping_add({});", i % 7),
                4 => "    next".to_owned(),
                _ => "}".to_owned(),
            }
        };
        old.push_str(&line);
        old.push('\n');
        new.push_str(&line);
        if i % 10 == 3 {
            new.push_str(" // edited");
        }
        new.push('\n');
    }
    (old, new)
}

/// What the bench has measured so far.
struct Bench {
    view: Entity<DiffView>,
    opened_at: Instant,
    step: usize,
    worst_draw: Duration,
    total_draw: Duration,
    worst_frame: Duration,
    last_frame: Instant,
}

fn open(model: DiffModel, cx: &mut App) {
    let offscreen = requested();
    let (width, height) = WINDOW_SIZE;
    let bounds = Bounds::centered(None, size(px(width.into()), px(height.into())), cx);
    let opened_at = Instant::now();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            focus: !offscreen,
            show: !offscreen,
            ..Default::default()
        },
        move |window, cx| {
            if offscreen {
                show_cloaked(window);
            }
            let view = cx.new(|cx| DiffView::new(model, FontSettings::default(), cx));
            let bench = Bench {
                view: view.clone(),
                opened_at,
                step: 0,
                worst_draw: Duration::ZERO,
                total_draw: Duration::ZERO,
                worst_frame: Duration::ZERO,
                last_frame: opened_at,
            };
            window.on_next_frame(move |window, cx| wait_for_first_draw(bench, window, cx));
            view
        },
    );
    match opened {
        Err(err) => {
            tracing::error!("opening the window: {err:#}");
            cx.quit();
        }
        Ok(_) if !offscreen => cx.activate(true),
        Ok(_) => {}
    }
}

/// Waits frame by frame until the view has drawn rows, then starts the
/// scroll steps.
fn wait_for_first_draw(bench: Bench, window: &mut Window, cx: &mut App) {
    if bench.view.read(cx).rendered_rows().is_empty() {
        window.refresh();
        window.on_next_frame(move |window, cx| wait_for_first_draw(bench, window, cx));
        return;
    }
    tracing::info!(first_draw = ?bench.opened_at.elapsed(), "window open to its first drawn rows");
    let bench = Bench {
        last_frame: Instant::now(),
        ..bench
    };
    scroll_step(bench, window, cx);
}

/// One scroll step: moves the list down, times a draw of the window, and
/// schedules the next step on the next frame; after the last, reports and
/// quits.
fn scroll_step(mut bench: Bench, window: &mut Window, cx: &mut App) {
    let now = Instant::now();
    if bench.step > 0 {
        bench.worst_frame = bench.worst_frame.max(now - bench.last_frame);
    }
    bench.last_frame = now;
    if bench.step == STEPS {
        tracing::info!(
            steps = STEPS,
            worst_draw = ?bench.worst_draw,
            mean_draw = ?bench.total_draw / u32::try_from(STEPS).unwrap_or(1),
            worst_frame = ?bench.worst_frame,
            "scroll steps"
        );
        cx.quit();
        return;
    }
    let (handle, rows) = {
        let view = bench.view.read(cx);
        (view.scroll_handle().clone(), view.model().rows().len())
    };
    handle.scroll_to_item_strict(
        (bench.step + 1) * STEP_ROWS % rows.max(1),
        ScrollStrategy::Top,
    );
    let started = Instant::now();
    window.draw(cx).clear();
    let took = started.elapsed();
    bench.worst_draw = bench.worst_draw.max(took);
    bench.total_draw += took;
    bench.step += 1;
    window.refresh();
    window.on_next_frame(move |window, cx| scroll_step(bench, window, cx));
}
