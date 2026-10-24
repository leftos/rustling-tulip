//! The prompts the owner sent, found in a pane's rows, and where the pane
//! header's previous and next prompt buttons scroll to.
//!
//! Claude draws each submitted prompt (a slash command included) as a row
//! that starts with `❯ ` at column 0, its own continuation rows indented two
//! spaces. Its live prompt box is a `❯` row right under a `─` rule, and a
//! dialog's highlighted option reads `❯ 1. …`; neither is a sent prompt.

/// What starts the first row of a prompt Claude echoed.
const PROMPT_MARK: &str = "❯ ";
/// What a box-drawing rule starts with: the live prompt box sits under one.
const RULE: char = '─';

/// One grid row as the scan reads it.
#[derive(Clone, Copy, Debug)]
pub struct Row<'a> {
    /// The row's text from column 0; its first few cells are enough.
    pub text: &'a str,
    /// Whether the row continues the row above it (a soft wrap).
    pub wrapped: bool,
}

/// Which way a prompt button scrolls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Previous,
    Next,
}

/// Where a prompt button scrolls to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Jump {
    /// This grid line goes to the top row.
    Line(i32),
    /// The live screen.
    Bottom,
}

/// The indexes of `rows` that start a prompt the owner sent, in order.
///
/// A row counts when it starts with `❯ ` and text, is not a soft-wrapped
/// continuation, is not a numbered dialog option (`❯ 1.`), and is not the
/// live prompt box (a `❯` row right under a `─` rule, with a draft in it or
/// not).
pub fn prompt_rows<'a>(rows: impl IntoIterator<Item = Row<'a>>) -> Vec<usize> {
    let mut found = Vec::new();
    let mut under_rule = false;
    for (index, row) in rows.into_iter().enumerate() {
        if !under_rule && is_prompt(row) {
            found.push(index);
        }
        under_rule = row.text.starts_with(RULE);
    }
    found
}

fn is_prompt(row: Row<'_>) -> bool {
    if row.wrapped {
        return false;
    }
    let Some(rest) = row.text.strip_prefix(PROMPT_MARK) else {
        return false;
    };
    !rest.trim().is_empty() && !is_option(rest)
}

/// Whether `rest`, the text after `❯ `, is a numbered dialog option: digits
/// and a dot.
fn is_option(rest: &str) -> bool {
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with('.')
}

/// Where `dir` scrolls a view whose top row is grid line `top`, given the
/// prompt lines `prompts` (oldest first) and whether the view is on the
/// live screen.
///
/// Previous goes to the nearest prompt strictly above the top row, and
/// nowhere from the oldest. Next goes to the nearest prompt strictly below
/// it, to the live screen from the newest, and nowhere while the view is
/// already there or the pane has no prompt.
#[must_use]
pub fn target(prompts: &[i32], top: i32, at_bottom: bool, dir: Dir) -> Option<Jump> {
    match dir {
        Dir::Previous => prompts
            .iter()
            .rev()
            .find(|line| **line < top)
            .map(|line| Jump::Line(*line)),
        Dir::Next if at_bottom || prompts.is_empty() => None,
        Dir::Next => Some(
            prompts
                .iter()
                .find(|line| **line > top)
                .map_or(Jump::Bottom, |line| Jump::Line(*line)),
        ),
    }
}

/// The display offset that puts grid line `line` at the top row: a history
/// line (negative) is that many lines back, and a screen line is on the
/// live screen.
#[must_use]
pub fn offset_for(line: i32) -> usize {
    if line < 0 {
        usize::try_from(line.unsigned_abs()).unwrap_or(usize::MAX)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V11: &str = include_str!("../../../crates/daemon/src/screen/fixtures/v11-turn1-end.txt");
    const V152: &str =
        include_str!("../../../crates/daemon/src/screen/fixtures/v152-learn-more.txt");
    const P98: &str = include_str!("../../../crates/daemon/src/screen/fixtures/p98-compacted.txt");
    const V21: &str = include_str!("../../../crates/daemon/src/screen/fixtures/v21-perm-bash.txt");
    const V31: &str = include_str!("../../../crates/daemon/src/screen/fixtures/v31-ask-q1.txt");

    /// The prompt rows of a fixture screen, 0-based.
    fn prompts_in(screen: &str) -> Vec<usize> {
        prompt_rows(screen.lines().map(|text| Row {
            text,
            wrapped: false,
        }))
    }

    #[test]
    fn prompt_rows_find_user_turns_and_skip_the_live_box() {
        assert_eq!(prompts_in(V11), [6], "line 7 is the turn; line 71 the box");
        assert_eq!(
            prompts_in("────\n❯ a draft not yet sent\n────"),
            Vec::<usize>::new(),
            "a draft in the live box is not sent"
        );
    }

    #[test]
    fn prompt_rows_skip_dialog_options() {
        assert_eq!(prompts_in(V31), [6, 34, 42, 49], "line 58 is `❯ 1. sub`");
        assert_eq!(prompts_in(V21), [6, 34], "line 49 is ` ❯ 1. Yes`");
    }

    #[test]
    fn prompt_rows_include_slash_commands() {
        assert_eq!(prompts_in(V152), [11, 14, 27], "line 12 is `❯ /model`");
        assert_eq!(prompts_in(P98), [6, 53, 60], "line 61 is `❯ /compact`");
    }

    #[test]
    fn prompt_rows_count_a_wrapped_prompt_once() {
        let claude_wrapped: Vec<&str> = V11.lines().skip(6).take(3).collect();
        assert!(claude_wrapped[1].starts_with("  "));
        assert_eq!(prompts_in(&claude_wrapped.join("\n")), [0]);
        let soft_wrapped = [
            Row {
                text: "❯ a prompt long enough that the terminal wrapped it, and",
                wrapped: false,
            },
            Row {
                text: "❯ the next row happens to start with the glyph",
                wrapped: true,
            },
        ];
        assert_eq!(prompt_rows(soft_wrapped), [0]);
    }

    #[test]
    fn target_steps_previous_to_the_prompt_above_the_top() {
        let prompts = [-90, -40, -10, 5];
        assert_eq!(
            target(&prompts, 0, true, Dir::Previous),
            Some(Jump::Line(-10)),
            "from the live screen: the newest prompt above it"
        );
        assert_eq!(
            target(&prompts, -10, false, Dir::Previous),
            Some(Jump::Line(-40)),
            "strictly above: the prompt on the top row is skipped"
        );
        assert_eq!(
            target(&prompts, -40, false, Dir::Next),
            Some(Jump::Line(-10))
        );
    }

    #[test]
    fn target_next_from_the_newest_is_bottom() {
        let prompts = [-90, -40];
        assert_eq!(target(&prompts, -40, false, Dir::Next), Some(Jump::Bottom));
        assert_eq!(
            target(&prompts, 0, true, Dir::Next),
            None,
            "nowhere while already on the live screen"
        );
        assert_eq!(
            target(&[], -40, false, Dir::Next),
            None,
            "or with no prompt"
        );
    }

    #[test]
    fn target_previous_at_the_first_is_none() {
        let prompts = [-90, -40];
        assert_eq!(target(&prompts, -90, false, Dir::Previous), None);
        assert_eq!(target(&[], 0, true, Dir::Previous), None);
        assert_eq!(
            target(&[3], 0, true, Dir::Previous),
            None,
            "a prompt on the screen is not above the top row"
        );
    }

    #[test]
    fn offset_for_puts_the_line_at_the_top() {
        assert_eq!(offset_for(-40), 40);
        assert_eq!(offset_for(0), 0);
        assert_eq!(offset_for(7), 0, "a screen line is on the live screen");
    }
}
