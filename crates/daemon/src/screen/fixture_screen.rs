//! The screen captures under `fixtures/` as grids, for the screen tests.

use std::sync::LazyLock;

use regex::Regex;

use super::Screen;

/// The CSI sequences a capture carries, for measuring a row's visible
/// width without them.
#[expect(
    clippy::expect_used,
    reason = "a fixed pattern, compiled in tests only"
)]
static CSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("the CSI pattern compiles"));

/// One capture as a screen: its lines are the rows, in a grid wide enough
/// for the widest of them.
pub(crate) fn screen_from(text: &str) -> Screen {
    grid_of(text, 2)
}

/// One capture as a screen exactly as tall as it was taken, for a test whose
/// reading depends on the screen's height (the prompt box's height cap).
pub(crate) fn screen_at_height(text: &str) -> Screen {
    grid_of(text, 0)
}

/// One capture fed into a grid wide enough for its widest line and
/// `spare_rows` taller than its line count.
#[expect(
    clippy::expect_used,
    reason = "a capture too big for a grid fails its test"
)]
fn grid_of(text: &str, spare_rows: usize) -> Screen {
    let lines: Vec<&str> = text.lines().collect();
    let widest = lines
        .iter()
        .map(|line| visible_width(line))
        .max()
        .unwrap_or(0);
    let columns = u16::try_from(widest + 4).expect("a capture fits a grid");
    let rows = u16::try_from(lines.len() + spare_rows).expect("a capture fits a grid");
    let mut screen = Screen::new(columns, rows);
    screen.feed(lines.join("\r\n").as_bytes());
    screen
}

/// How many cells a line shows, its escape sequences aside.
fn visible_width(line: &str) -> usize {
    CSI.replace_all(line, "").chars().count()
}
