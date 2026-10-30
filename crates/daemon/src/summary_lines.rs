//! The summary view's lines: a session's chat split into stretches, one per
//! turn and, for a long turn, one per window of its activity, each line
//! written from a template.
//!
//! A turn starts at the entry that began it and runs to the next turn's
//! start; entries before the first turn join no stretch. A turn spanning
//! under [`SHORT_TURN`], or whose times are unknown, is one line. A longer
//! one splits by each entry's offset from the turn's first time into the
//! windows `[0,5m)`, `[5m,15m)`, `[15m,30m)` and then 30 minutes each.

use chrono::{DateTime, TimeDelta, Utc};
use protocol::{ChatEntry, StretchLine, SummarySource};

use crate::chat_mirror::entry_id;
use crate::summarizer::prompt::shape;
use crate::summarizer::reply::cut_chars;

/// The span from which a turn splits into windows.
const SHORT_TURN: TimeDelta = TimeDelta::minutes(5);

/// Where each of the first windows ends, as an offset from the turn's first
/// time.
const FIRST_WINDOW_ENDS: [TimeDelta; 3] = [
    TimeDelta::minutes(5),
    TimeDelta::minutes(15),
    TimeDelta::minutes(30),
];

/// Where the first windows end and the later windows start.
const LATER_WINDOWS_START: TimeDelta = TimeDelta::minutes(30);

/// How long each window after the first ones is.
const LATER_WINDOW: TimeDelta = TimeDelta::minutes(30);

/// The most characters a line's leading text keeps.
const TEXT_MAX_CHARS: usize = 100;

/// One chat entry with what the stretches need of it beside its kind.
pub struct TimedEntry<'a> {
    pub entry: &'a ChatEntry,
    /// The entry's own time, when its transcript line carried one.
    pub at: Option<DateTime<Utc>>,
    /// Whether the entry began a turn.
    pub starts_turn: bool,
}

/// An entry of a turn with its id and the window it falls in.
struct Placed<'a> {
    id: &'a str,
    entry: &'a ChatEntry,
    at: Option<DateTime<Utc>>,
    window: usize,
}

/// The stretch lines of a chat whose entries are `entries`, in chat order.
pub fn stretch_lines(entries: &[TimedEntry<'_>]) -> Vec<StretchLine> {
    turns(entries).into_iter().flat_map(turn_lines).collect()
}

/// Each stretch of a chat whose entries are `entries`, in chat order, as its
/// id and the text the summarizer is sent for it: the opening user entry (or
/// the row that began the turn) for a turn's first window, then each tool's
/// summary and each answer's text, one per line, shaped as every summary
/// input is. A pending user entry is left out, and nothing in it depends on
/// the stretch's times.
pub fn stretch_inputs(entries: &[TimedEntry<'_>]) -> Vec<(String, String)> {
    turns(entries).into_iter().flat_map(turn_inputs).collect()
}

/// The chat's turns: each runs from an entry that began a turn to the next
/// one, the last to the chat's end.
fn turns<'e, 'a>(entries: &'e [TimedEntry<'a>]) -> Vec<&'e [TimedEntry<'a>]> {
    let starts: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, timed)| timed.starts_turn)
        .map(|(at, _)| at)
        .collect();
    starts
        .iter()
        .enumerate()
        .filter_map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(entries.len());
            entries.get(start..end)
        })
        .collect()
}

/// One turn's lines: one per window that holds an entry.
fn turn_lines(turn: &[TimedEntry<'_>]) -> Vec<StretchLine> {
    let placed = place(turn);
    let Some(turn_id) = placed.first().map(|first| first.id) else {
        return Vec::new();
    };
    let prompt = opening(&placed, |_| true);
    placed
        .chunk_by(|a, b| a.window == b.window)
        .filter_map(|window| window_line(turn_id, prompt.unwrap_or_default(), window))
        .collect()
}

/// One turn's stretch inputs: one per window that holds an entry.
fn turn_inputs(turn: &[TimedEntry<'_>]) -> Vec<(String, String)> {
    let placed = place(turn);
    let Some(turn_id) = placed.first().map(|first| first.id) else {
        return Vec::new();
    };
    let lead = opening(&placed, |pending| !pending);
    placed
        .chunk_by(|a, b| a.window == b.window)
        .filter_map(|window| {
            let first = window.first()?;
            let mut parts: Vec<&str> = Vec::new();
            if first.window == 0 {
                parts.extend(lead);
            }
            parts.extend(window.iter().filter_map(|placed| match placed.entry {
                ChatEntry::Tool { summary, .. } => Some(summary.as_str()),
                ChatEntry::Answer { text, .. } => Some(text.as_str()),
                _ => None,
            }));
            let input = shape(&[&parts.join("\n")]);
            Some((stretch_id(turn_id, first.window), input))
        })
        .collect()
}

/// What a turn opens with: the row that began it (a `!` command, a notice),
/// else its first user entry whose pending flag `keep` accepts.
fn opening<'a>(placed: &[Placed<'a>], keep: impl Fn(bool) -> bool) -> Option<&'a str> {
    match placed.first().map(|first| first.entry) {
        Some(ChatEntry::System { text, .. }) => Some(text.as_str()),
        _ => placed.iter().find_map(|placed| match placed.entry {
            ChatEntry::User { text, pending, .. } if keep(*pending) => Some(text.as_str()),
            _ => None,
        }),
    }
}

/// The id of window `window` of turn `turn_id`: the turn's own id for the
/// first window, `<turn id>#<n>` for a later one.
fn stretch_id(turn_id: &str, window: usize) -> String {
    if window == 0 {
        turn_id.to_owned()
    } else {
        format!("{turn_id}#{window}")
    }
}

/// The turn's entries that have an id, each with its window: all in the
/// first one for a short or untimed turn. An entry with no time stays in the
/// window of the entry before it, and a window never goes back, so each
/// window's entries are one run of the chat.
fn place<'a>(turn: &[TimedEntry<'a>]) -> Vec<Placed<'a>> {
    let first = turn.iter().find_map(|timed| timed.at);
    let last = turn.iter().rev().find_map(|timed| timed.at);
    let origin = first
        .zip(last)
        .and_then(|(first, last)| (last - first >= SHORT_TURN).then_some(first));
    let mut window = 0;
    turn.iter()
        .filter_map(|timed| {
            let id = entry_id(timed.entry)?;
            if let (Some(origin), Some(at)) = (origin, timed.at) {
                window = window.max(window_of(at - origin));
            }
            Some(Placed {
                id,
                entry: timed.entry,
                at: timed.at,
                window,
            })
        })
        .collect()
}

/// The index of the window an entry `offset` after its turn's first time
/// falls in.
fn window_of(offset: TimeDelta) -> usize {
    if let Some(first) = FIRST_WINDOW_ENDS.iter().position(|end| offset < *end) {
        return first;
    }
    let later = (offset - LATER_WINDOWS_START).num_seconds() / LATER_WINDOW.num_seconds();
    // An offset past usize's range cannot happen on a 64-bit target.
    FIRST_WINDOW_ENDS.len() + usize::try_from(later).unwrap_or(usize::MAX - FIRST_WINDOW_ENDS.len())
}

/// The line of one window of turn `turn_id`, whose first user entry reads
/// `prompt`.
fn window_line(turn_id: &str, prompt: &str, window: &[Placed<'_>]) -> Option<StretchLine> {
    let first = window.first()?;
    let last = window.last()?;
    let id = stretch_id(turn_id, first.window);
    let lead = if first.window == 0 {
        prompt
    } else {
        later_lead(window)
    };
    let started_at = window.iter().find_map(|placed| placed.at);
    let ended_at = window.iter().rev().find_map(|placed| placed.at);
    let tools = window
        .iter()
        .filter(|placed| matches!(placed.entry, ChatEntry::Tool { .. }))
        .count();
    let duration = started_at.zip(ended_at).map(|(start, end)| end - start);
    Some(StretchLine {
        id,
        from_entry: first.id.to_owned(),
        to_entry: last.id.to_owned(),
        started_at,
        ended_at,
        text: template_text(lead, duration, tools),
        source: SummarySource::Template,
    })
}

/// What a later window's line starts with: its first tool's summary, else
/// its first answer, else nothing.
fn later_lead<'a>(window: &[Placed<'a>]) -> &'a str {
    let tool = window.iter().find_map(|placed| match placed.entry {
        ChatEntry::Tool { summary, .. } => Some(summary.as_str()),
        _ => None,
    });
    let answer = || {
        window.iter().find_map(|placed| match placed.entry {
            ChatEntry::Answer { text, .. } => Some(text.as_str()),
            _ => None,
        })
    };
    tool.or_else(answer).unwrap_or_default()
}

/// `lead`'s first line, cut to [`TEXT_MAX_CHARS`], then `(<duration>, <n>
/// tools)`, leaving out an unknown duration, zero tools, and the brackets
/// when both are out.
fn template_text(lead: &str, duration: Option<TimeDelta>, tools: usize) -> String {
    let lead = cut_chars(lead.lines().next().unwrap_or_default(), TEXT_MAX_CHARS);
    let mut stats = Vec::new();
    if let Some(duration) = duration {
        stats.push(duration_text(duration));
    }
    match tools {
        0 => {}
        1 => stats.push("1 tool".to_owned()),
        n => stats.push(format!("{n} tools")),
    }
    let stats = (!stats.is_empty()).then(|| format!("({})", stats.join(", ")));
    [Some(lead).filter(|lead| !lead.is_empty()), stats]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `duration` as a line shows it: `<1m`, `4m`, `1h 5m`.
fn duration_text(duration: TimeDelta) -> String {
    let minutes = duration.num_minutes();
    if duration.num_seconds() < 60 {
        "<1m".to_owned()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h {}m", minutes / 60, minutes % 60)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    /// One chat entry as a test writes it: the entry, its minute (when it
    /// has a time) and whether it starts a turn.
    type Row = (ChatEntry, Option<i64>, bool);

    fn base() -> DateTime<Utc> {
        "2026-10-30T10:00:00Z".parse().expect("a valid time")
    }

    fn minute(n: i64) -> DateTime<Utc> {
        base() + TimeDelta::minutes(n)
    }

    fn prompt(id: &str, text: &str, at: Option<i64>) -> Row {
        let entry = ChatEntry::User {
            id: id.to_owned(),
            text: text.to_owned(),
            pending: false,
        };
        (entry, at, true)
    }

    fn tool(id: &str, at: Option<i64>) -> Row {
        let entry = ChatEntry::Tool {
            id: id.to_owned(),
            name: "Bash".to_owned(),
            summary: format!("ran {id}"),
            input: String::new(),
            result: None,
            parent_id: None,
        };
        (entry, at, false)
    }

    fn answer(id: &str, at: Option<i64>) -> Row {
        let entry = ChatEntry::Answer {
            id: id.to_owned(),
            text: format!("answer {id}\nsecond line"),
            live: false,
        };
        (entry, at, false)
    }

    fn lines_of(rows: &[Row]) -> Vec<StretchLine> {
        let timed: Vec<TimedEntry<'_>> = rows
            .iter()
            .map(|(entry, at, starts_turn)| TimedEntry {
                entry,
                at: at.map(minute),
                starts_turn: *starts_turn,
            })
            .collect();
        stretch_lines(&timed)
    }

    fn inputs_of(rows: &[Row]) -> Vec<(String, String)> {
        let timed: Vec<TimedEntry<'_>> = rows
            .iter()
            .map(|(entry, at, starts_turn)| TimedEntry {
                entry,
                at: at.map(minute),
                starts_turn: *starts_turn,
            })
            .collect();
        stretch_inputs(&timed)
    }

    fn ids(lines: &[StretchLine]) -> Vec<&str> {
        lines.iter().map(|line| line.id.as_str()).collect()
    }

    fn row_ids(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .filter_map(|(entry, _, _)| entry_id(entry).map(str::to_owned))
            .collect()
    }

    /// The entry ids each line covers, expanded through the chat's order.
    fn expanded(rows: &[Row], line: &StretchLine) -> Vec<String> {
        let all = row_ids(rows);
        let from = all
            .iter()
            .position(|id| *id == line.from_entry)
            .expect("from_entry is in the chat");
        let to = all
            .iter()
            .position(|id| *id == line.to_entry)
            .expect("to_entry is in the chat");
        all.get(from..=to).expect("from is not after to").to_vec()
    }

    #[test]
    fn one_line_per_short_turn() {
        let rows = [
            prompt("p1", "Fix the login bug\nand the tests", Some(0)),
            tool("t1", Some(1)),
            answer("a1", Some(2)),
            prompt("p2", "Now the docs", Some(10)),
            answer("a2", Some(13)),
        ];
        let lines = lines_of(&rows);
        assert_eq!(ids(&lines), ["p1", "p2"]);
        assert_eq!(lines[0].text, "Fix the login bug (2m, 1 tool)");
        assert_eq!(lines[1].text, "Now the docs (3m)");
        assert_eq!(
            (lines[0].from_entry.as_str(), lines[0].to_entry.as_str()),
            ("p1", "a1")
        );
        assert_eq!(
            (lines[0].started_at, lines[0].ended_at),
            (Some(minute(0)), Some(minute(2)))
        );
        assert!(
            lines
                .iter()
                .all(|line| line.source == SummarySource::Template)
        );
    }

    #[test]
    fn a_long_turn_splits_into_windows() {
        let rows = [
            prompt("p", "Port the parser", Some(0)),
            tool("t3", Some(3)),
            tool("t7", Some(7)),
            answer("a20", Some(20)),
            tool("t40", Some(40)),
        ];
        let lines = lines_of(&rows);
        assert_eq!(ids(&lines), ["p", "p#1", "p#2", "p#3"]);
        let covered: Vec<Vec<String>> = lines.iter().map(|line| expanded(&rows, line)).collect();
        assert_eq!(
            covered,
            [
                vec!["p".to_owned(), "t3".to_owned()],
                vec!["t7".to_owned()],
                vec!["a20".to_owned()],
                vec!["t40".to_owned()],
            ]
        );
        assert_eq!(lines[0].text, "Port the parser (3m, 1 tool)");
        assert_eq!(lines[1].text, "ran t7 (<1m, 1 tool)");
        assert_eq!(lines[2].text, "answer a20 (<1m)");
    }

    #[test]
    fn each_line_expands_to_its_entries() {
        let rows = [
            answer("before", None),
            prompt("p1", "One", Some(0)),
            tool("t1", Some(1)),
            answer("a1", None),
            tool("t2", Some(6)),
            answer("a2", Some(16)),
            prompt("p2", "Two", Some(20)),
            answer("a3", None),
            prompt("p3", "Three", None),
        ];
        let lines = lines_of(&rows);
        assert_eq!(ids(&lines), ["p1", "p1#1", "p1#2", "p2", "p3"]);
        let joined: Vec<String> = lines
            .iter()
            .flat_map(|line| expanded(&rows, line))
            .collect();
        let turn_entries: Vec<String> = row_ids(&rows).into_iter().skip(1).collect();
        assert_eq!(joined, turn_entries, "no gap, no overlap, in chat order");
    }

    #[test]
    fn entries_before_the_first_prompt_join_no_stretch() {
        let head = ChatEntry::System {
            id: "cleared:c2".to_owned(),
            text: "Conversation cleared".to_owned(),
        };
        let rows = [
            (head, None, false),
            answer("early", Some(0)),
            prompt("p", "Go", Some(1)),
        ];
        let lines = lines_of(&rows);
        assert_eq!(ids(&lines), ["p"]);
        assert_eq!(lines[0].from_entry, "p");
        assert_eq!(
            lines_of(&rows[..2]),
            Vec::new(),
            "a chat with no prompt has no stretch"
        );
    }

    #[test]
    fn unknown_times_make_one_line() {
        let rows = [
            prompt("p", "Go", None),
            tool("t1", None),
            answer("a1", None),
        ];
        let lines = lines_of(&rows);
        assert_eq!(ids(&lines), ["p"]);
        assert_eq!(
            lines[0].text, "Go (1 tool)",
            "an unknown duration is left out"
        );
        assert_eq!((lines[0].started_at, lines[0].ended_at), (None, None));
        assert_eq!(lines[0].to_entry, "a1");

        let untimed_prompt = [prompt("p", "Go", None)];
        assert_eq!(lines_of(&untimed_prompt)[0].text, "Go");
    }

    #[test]
    fn stretch_inputs_follow_the_windows() {
        let head = ChatEntry::System {
            id: "bang".to_owned(),
            text: "!cargo test".to_owned(),
        };
        let rows = [
            answer("before", None),
            prompt("p1", "One", Some(0)),
            tool("t1", Some(1)),
            answer("a1", None),
            tool("t2", Some(6)),
            answer("a2", Some(16)),
            prompt("p2", "Two", Some(20)),
            answer("a3", None),
            (head, Some(21), true),
            tool("t3", Some(22)),
        ];
        let inputs = inputs_of(&rows);
        let input_ids: Vec<&str> = inputs.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(input_ids, ids(&lines_of(&rows)));
        let texts: Vec<&str> = inputs.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "One\nran t1\nanswer a1\nsecond line",
                "ran t2",
                "answer a2\nsecond line",
                "Two\nanswer a3\nsecond line",
                "!cargo test\nran t3",
            ]
        );

        let pending = ChatEntry::User {
            id: "q".to_owned(),
            text: "Typed, not sent".to_owned(),
            pending: true,
        };
        let rows = [(pending, None, true), tool("t4", None)];
        assert_eq!(
            inputs_of(&rows),
            [("q".to_owned(), "ran t4".to_owned())],
            "a pending user entry is left out"
        );
    }

    #[test]
    fn the_input_ignores_duration() {
        let rows = vec![
            prompt("p", "Go", Some(0)),
            tool("t1", Some(1)),
            tool("t2", Some(7)),
        ];
        let before = inputs_of(&rows);
        assert_eq!(before[0], ("p".to_owned(), "Go\nran t1".to_owned()));

        let mut later_window = rows.clone();
        later_window.push(tool("t3", Some(12)));
        assert_eq!(inputs_of(&later_window)[0], before[0]);

        let mut next_turn = later_window;
        next_turn.push(prompt("n", "Next", Some(50)));
        next_turn.push(answer("a", Some(51)));
        assert_eq!(inputs_of(&next_turn)[0], before[0]);
    }

    #[test]
    fn durations_read() {
        assert_eq!(duration_text(TimeDelta::seconds(59)), "<1m");
        assert_eq!(duration_text(TimeDelta::seconds(4 * 60 + 59)), "4m");
        assert_eq!(duration_text(TimeDelta::minutes(65)), "1h 5m");
        assert_eq!(duration_text(TimeDelta::minutes(120)), "2h 0m");
    }

    #[test]
    fn text_is_cut_to_100_chars() {
        let long = "word ".repeat(40);
        let rows = [prompt("p", &long, None)];
        let text = &lines_of(&rows)[0].text;
        assert!(text.chars().count() <= TEXT_MAX_CHARS, "{text}");
        assert!(text.chars().count() > TEXT_MAX_CHARS - 6, "{text}");
        assert!(text.starts_with("word word"), "{text}");
    }
}
