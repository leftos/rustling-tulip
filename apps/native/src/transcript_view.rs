//! A followed dispatch run's transcript, drawn the way the agent draws a
//! session in a terminal: each answer and tool call under a `●`, a tool's
//! output under it after `⎿`, cut to its first lines until clicked. The
//! model is plain Rust, so every rule is unit-tested; `RootView` keeps one
//! per followed run and draws its lines.

use std::collections::HashSet;

use protocol::ChatEntry;

/// The transcript a client holds of one followed run.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Transcript {
    pub entries: Vec<ChatEntry>,
    /// Entries the daemon dropped from the front of its log.
    pub hidden: u32,
    pub ended: bool,
    /// The ids of the tool calls whose whole output is drawn.
    pub expanded: HashSet<String>,
}

impl Transcript {
    /// Takes a whole log in place of the one held; the expanded calls stay.
    pub(crate) fn replace(&mut self, entries: Vec<ChatEntry>, hidden: u32) {
        self.entries = entries;
        self.hidden = hidden;
    }

    /// Folds in `entries`: one whose id is held replaces it in place, any
    /// other goes at the end. Marks the run ended when `ended`.
    pub(crate) fn append(&mut self, entries: Vec<ChatEntry>, ended: bool) {
        for entry in entries {
            let held = entry_id(&entry)
                .and_then(|id| self.entries.iter().position(|e| entry_id(e) == Some(id)));
            match held {
                Some(at) => self.entries[at] = entry,
                None => self.entries.push(entry),
            }
        }
        self.ended |= ended;
    }

    /// Expands the output of tool call `id`, or collapses it again.
    pub(crate) fn toggle(&mut self, id: &str) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_owned());
        }
    }
}

/// The id of `entry`; `None` for a kind this build does not know.
fn entry_id(entry: &ChatEntry) -> Option<&str> {
    match entry {
        ChatEntry::User { id, .. }
        | ChatEntry::Answer { id, .. }
        | ChatEntry::Thinking { id, .. }
        | ChatEntry::Tool { id, .. }
        | ChatEntry::System { id, .. }
        | ChatEntry::Receipt { id, .. }
        | ChatEntry::TurnEnd { id, .. } => Some(id),
        ChatEntry::Unknown => None,
    }
}

/// The output lines a collapsed tool call draws before its count.
const COLLAPSED_LINES: usize = 5;

/// How a transcript line is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    Answer,
    Tool,
    Output,
    Error,
    Muted,
}

/// One drawn line of a transcript; `entry` names the tool call a click on
/// it expands or collapses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptLine {
    pub text: String,
    pub kind: LineKind,
    pub entry: Option<String>,
}

/// The lines `transcript` draws, in entry order.
pub(crate) fn transcript_lines(transcript: &Transcript) -> Vec<TranscriptLine> {
    let mut lines = Vec::new();
    if transcript.hidden > 0 {
        let note = format!("… {} earlier entries not shown", transcript.hidden);
        lines.push(plain(note, LineKind::Muted));
    }
    for entry in &transcript.entries {
        match entry {
            ChatEntry::Answer { text, .. } => answer_lines(text, &mut lines),
            ChatEntry::Tool {
                id,
                name,
                summary,
                result,
                ..
            } => {
                let call = ToolCall {
                    id,
                    name,
                    summary,
                    result: result.as_ref(),
                    expanded: transcript.expanded.contains(id),
                };
                tool_lines(&call, &mut lines);
            }
            ChatEntry::System { text, .. } => lines.push(plain(text.clone(), LineKind::Muted)),
            _ => {}
        }
    }
    lines
}

/// A line no click acts on.
fn plain(text: String, kind: LineKind) -> TranscriptLine {
    TranscriptLine {
        text,
        kind,
        entry: None,
    }
}

/// An answer's lines: the first after `●`, the rest indented under it.
fn answer_lines(text: &str, lines: &mut Vec<TranscriptLine>) {
    let mut rows = text.lines();
    let first = rows.next().unwrap_or_default();
    lines.push(plain(format!("● {first}"), LineKind::Answer));
    lines.extend(rows.map(|row| plain(format!("  {row}"), LineKind::Answer)));
}

/// A tool call as its lines draw it.
struct ToolCall<'a> {
    id: &'a str,
    name: &'a str,
    summary: &'a str,
    result: Option<&'a protocol::ToolOutcome>,
    expanded: bool,
}

/// A tool call's lines: the call, then its output under `⎿`, or `Running…`
/// while it has none; each carries the call's id.
fn tool_lines(call: &ToolCall<'_>, lines: &mut Vec<TranscriptLine>) {
    let line = |text: String, kind: LineKind| TranscriptLine {
        text,
        kind,
        entry: Some(call.id.to_owned()),
    };
    let head = if call.summary.is_empty() {
        format!("● {}", call.name)
    } else {
        format!("● {}({})", call.name, call.summary)
    };
    lines.push(line(head, LineKind::Tool));
    let Some(result) = call.result else {
        lines.push(line("  ⎿  Running…".to_owned(), LineKind::Muted));
        return;
    };
    let kind = if result.is_error {
        LineKind::Error
    } else {
        LineKind::Output
    };
    let rows: Vec<&str> = result.text.lines().collect();
    let shown = if call.expanded {
        rows.len()
    } else {
        rows.len().min(COLLAPSED_LINES)
    };
    for (at, row) in rows.iter().take(shown).enumerate() {
        let lead = if at == 0 { "  ⎿  " } else { "     " };
        lines.push(line(format!("{lead}{row}"), kind));
    }
    if shown < rows.len() {
        let more = format!("     … +{} lines", rows.len() - shown);
        lines.push(line(more, LineKind::Muted));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{ToolKind, ToolOutcome};

    fn answer(id: &str, text: &str) -> ChatEntry {
        ChatEntry::Answer {
            id: id.to_owned(),
            text: text.to_owned(),
            live: false,
        }
    }

    fn tool(id: &str, summary: &str, result: Option<(&str, bool)>) -> ChatEntry {
        ChatEntry::Tool {
            id: id.to_owned(),
            name: "Bash".to_owned(),
            summary: summary.to_owned(),
            input: String::new(),
            result: result.map(|(text, is_error)| ToolOutcome {
                text: text.to_owned(),
                is_error,
            }),
            parent_id: None,
            tail_paths: Vec::new(),
            kind: ToolKind::Run,
        }
    }

    fn held(entries: Vec<ChatEntry>) -> Transcript {
        let mut transcript = Transcript::default();
        transcript.replace(entries, 0);
        transcript
    }

    fn texts(transcript: &Transcript) -> Vec<String> {
        transcript_lines(transcript)
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    fn line(text: &str, kind: LineKind, entry: Option<&str>) -> TranscriptLine {
        TranscriptLine {
            text: text.to_owned(),
            kind,
            entry: entry.map(str::to_owned),
        }
    }

    const SEVEN: &str = "l1\nl2\nl3\nl4\nl5\nl6\nl7";

    #[test]
    fn replace_sets_entries_and_hidden_and_keeps_expanded() {
        let mut transcript = held(vec![answer("a1", "old")]);
        transcript.toggle("t1");

        transcript.replace(vec![answer("a2", "new")], 7);

        assert_eq!(transcript.entries, vec![answer("a2", "new")]);
        assert_eq!(transcript.hidden, 7);
        assert!(transcript.expanded.contains("t1"), "expanded calls stay");
    }

    #[test]
    fn append_replaces_a_known_id_in_place_and_adds_new_ones() {
        let mut transcript = held(vec![tool("t1", "ls", None), answer("a1", "hi")]);

        transcript.append(
            vec![tool("t1", "ls", Some(("out", false))), answer("a2", "bye")],
            false,
        );

        assert_eq!(
            transcript.entries,
            vec![
                tool("t1", "ls", Some(("out", false))),
                answer("a1", "hi"),
                answer("a2", "bye"),
            ]
        );
        assert!(!transcript.ended);
    }

    #[test]
    fn append_marks_ended() {
        let mut transcript = held(Vec::new());
        transcript.append(Vec::new(), true);
        assert!(transcript.ended);
        transcript.append(Vec::new(), false);
        assert!(transcript.ended, "an ended run stays ended");
    }

    #[test]
    fn an_answer_draws_with_a_bullet_and_indents_its_further_lines() {
        let transcript = held(vec![answer("a1", "first\nsecond")]);
        assert_eq!(
            transcript_lines(&transcript),
            vec![
                line("● first", LineKind::Answer, None),
                line("  second", LineKind::Answer, None),
            ]
        );
    }

    #[test]
    fn a_running_tool_shows_running() {
        let transcript = held(vec![tool("t1", "cargo test", None)]);
        assert_eq!(
            transcript_lines(&transcript),
            vec![
                line("● Bash(cargo test)", LineKind::Tool, Some("t1")),
                line("  ⎿  Running…", LineKind::Muted, Some("t1")),
            ]
        );
    }

    #[test]
    fn a_tool_shows_five_output_lines_then_a_count() {
        let transcript = held(vec![tool("t1", "ls", Some((SEVEN, false)))]);
        assert_eq!(
            transcript_lines(&transcript),
            vec![
                line("● Bash(ls)", LineKind::Tool, Some("t1")),
                line("  ⎿  l1", LineKind::Output, Some("t1")),
                line("     l2", LineKind::Output, Some("t1")),
                line("     l3", LineKind::Output, Some("t1")),
                line("     l4", LineKind::Output, Some("t1")),
                line("     l5", LineKind::Output, Some("t1")),
                line("     … +2 lines", LineKind::Muted, Some("t1")),
            ]
        );
    }

    #[test]
    fn an_expanded_tool_shows_all_its_output() {
        let mut transcript = held(vec![tool("t1", "ls", Some((SEVEN, false)))]);
        transcript.toggle("t1");
        assert_eq!(
            texts(&transcript),
            [
                "● Bash(ls)",
                "  ⎿  l1",
                "     l2",
                "     l3",
                "     l4",
                "     l5",
                "     l6",
                "     l7"
            ]
        );
        transcript.toggle("t1");
        assert_eq!(texts(&transcript).len(), 7, "a second toggle collapses it");
    }

    #[test]
    fn a_failed_tool_draws_its_output_as_error() {
        let transcript = held(vec![tool("t1", "ls", Some(("no\nsuch", true)))]);
        assert_eq!(
            transcript_lines(&transcript),
            vec![
                line("● Bash(ls)", LineKind::Tool, Some("t1")),
                line("  ⎿  no", LineKind::Error, Some("t1")),
                line("     such", LineKind::Error, Some("t1")),
            ]
        );
    }

    #[test]
    fn thinking_and_turn_end_draw_nothing() {
        let transcript = held(vec![
            ChatEntry::Thinking {
                id: "k1".to_owned(),
                secs: Some(3),
            },
            ChatEntry::TurnEnd {
                id: "e1".to_owned(),
                duration_ms: Some(10),
            },
            ChatEntry::System {
                id: "s1".to_owned(),
                text: "cleared".to_owned(),
            },
        ]);
        assert_eq!(
            transcript_lines(&transcript),
            vec![line("cleared", LineKind::Muted, None)],
            "only the system row draws"
        );
    }

    #[test]
    fn hidden_entries_draw_a_first_note() {
        let mut transcript = Transcript::default();
        transcript.replace(vec![answer("a1", "hi")], 12);
        assert_eq!(
            transcript_lines(&transcript),
            vec![
                line("… 12 earlier entries not shown", LineKind::Muted, None),
                line("● hi", LineKind::Answer, None),
            ]
        );
    }

    #[test]
    fn an_empty_summary_draws_just_the_tool_name() {
        let transcript = held(vec![tool("t1", "", None)]);
        assert_eq!(
            texts(&transcript).first().map(String::as_str),
            Some("● Bash")
        );
    }
}
