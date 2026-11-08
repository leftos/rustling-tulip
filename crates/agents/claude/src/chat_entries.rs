//! Claude Code transcript records turned into chat entries.
//!
//! Each transcript line is one JSON record. A record's entries keep the
//! record's place in the file; ordering them across records is the mirror's.
//! Ids: a message's entries use the record `uuid`, a tool card the `tool_use`
//! block's id (so its result upserts the card), a receipt `receipt:<that id>`.

use agent_api::TranscriptLineFacts;
use chrono::{DateTime, Utc};
use protocol::{ChatEntry, ToolKind, ToolOutcome};
use serde_json::{Map, Value};

use crate::records;
use crate::tail_paths::{Shell, tail_paths};
use crate::transcripts::{is_command_noise, is_meta};

/// The most bytes of a tool call's input a card keeps.
const TOOL_INPUT_CAP: usize = 4096;
/// The most bytes of a tool call's result, or of a `!` command's output, a
/// row keeps.
const TOOL_RESULT_CAP: usize = 8192;
/// The longest tool card summary, in characters, including the ellipsis.
const SUMMARY_MAX_CHARS: usize = 120;
/// What a cut text ends with.
const CUT_MARKER: char = '…';
/// The prefixes of a notification summary that already reads as a sentence.
const SENTENCE_PREFIXES: [&str; 2] = ["Agent \"", "Background command \""];
/// The question a plan approval's receipt answers: the dialog's title.
const PLAN_QUESTION: &str = "Ready to code?";
const PLAN_ANSWER: &str = "Approved the plan";

type Known<'a> = &'a dyn Fn(&str) -> Option<ChatEntry>;

/// What one complete transcript line adds to the chat, in record order.
/// `known` answers the entry already held under an id: a tool result comes
/// back as its whole card with the result set, and a `!` command's output as
/// its row with the output appended. A line that does not parse, has no
/// `uuid`, or is not a message yields nothing.
pub(crate) fn chat_entries(line: &str, known: Known<'_>) -> Vec<ChatEntry> {
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some(uuid) = str_at(&record, "uuid").filter(|uuid| !uuid.is_empty()) else {
        return Vec::new();
    };
    match str_at(&record, "type") {
        Some("user") => user_entries(&record, uuid, known),
        Some("assistant") => assistant_entries(&record, uuid),
        Some("attachment") => queued_entry(&record, uuid).into_iter().collect(),
        Some("system") => system_entry(&record, uuid).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The prompt id, the record's time, whether one transcript line is a prompt
/// sent to Claude, and whether it is the agent's notice that the user
/// interrupted a turn: a `user` record carrying text (a string, or a text
/// block beside any tool results) is a prompt unless it is meta, a compact
/// summary or an interrupt notice, which is the interrupt fact instead. A line
/// that does not parse has none of them; a `timestamp` that is missing or not
/// RFC 3339 is no time.
pub(crate) fn line_facts(line: &str) -> TranscriptLineFacts {
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return TranscriptLineFacts::default();
    };
    let prompt = prompt_text(&record);
    let is_interrupt = prompt.as_deref().is_some_and(is_interrupt_text);
    TranscriptLineFacts {
        prompt_id: str_at(&record, "promptId").map(str::to_owned),
        is_user_prompt: prompt.is_some() && !is_interrupt,
        is_interrupt,
        timestamp: str_at(&record, "timestamp")
            .and_then(|stamp| DateTime::parse_from_rfc3339(stamp).ok())
            .map(|stamp| stamp.with_timezone(&Utc)),
        ended_tasks: ended_tasks(&record),
    }
}

/// The tasks a task-notification record reports ended: each notification's
/// task id. A monitor's event arrives as a notification too, carrying an
/// `<event>`, and leaves its monitor running, so that notification is not
/// listed; the others in the same record are.
fn ended_tasks(record: &Value) -> Vec<String> {
    if record.pointer("/origin/kind").and_then(Value::as_str) != Some("task-notification") {
        return Vec::new();
    }
    let Some(text) = record.pointer("/message/content").and_then(content_text) else {
        return Vec::new();
    };
    text.split_inclusive("</task-notification>")
        .filter(|own| tag_text(&without_tag(own, "result"), "event").is_none())
        .flat_map(records::task_notifications)
        .map(|notification| notification.task_id.to_owned())
        .collect()
}

/// The text a `user` record carries, which is what makes it a prompt: a
/// string, or an array's text blocks joined. A record that is not a user
/// record, is meta, is a compact summary, or has no text carries none.
fn prompt_text(record: &Value) -> Option<String> {
    if str_at(record, "type") != Some("user") || is_meta(record) || flag(record, "isCompactSummary")
    {
        return None;
    }
    let content = record.pointer("/message/content")?;
    let has_text = match content {
        Value::String(_) => true,
        Value::Array(blocks) => blocks
            .iter()
            .any(|block| str_at(block, "type") == Some("text")),
        _ => false,
    };
    if !has_text {
        return None;
    }
    content_text(content)
}

/// Whether `text` is Claude Code's notice that the user interrupted a turn.
fn is_interrupt_text(text: &str) -> bool {
    text.trim_start().starts_with(records::INTERRUPTED_PREFIX)
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

fn system_row(id: &str, text: String) -> ChatEntry {
    ChatEntry::System {
        id: id.to_owned(),
        text,
    }
}

fn user_entries(record: &Value, uuid: &str, known: Known<'_>) -> Vec<ChatEntry> {
    if is_meta(record) || flag(record, "isCompactSummary") {
        return Vec::new();
    }
    let Some(content) = record.pointer("/message/content") else {
        return Vec::new();
    };
    if record.pointer("/origin/kind").and_then(Value::as_str) == Some("task-notification") {
        return content_text(content)
            .and_then(|text| task_notification_row(&text))
            .map(|text| system_row(uuid, text))
            .into_iter()
            .collect();
    }
    match content {
        Value::String(text) => user_string_entry(record, uuid, text, known)
            .into_iter()
            .collect(),
        Value::Array(blocks) => user_block_entries(record, uuid, blocks, known),
        _ => Vec::new(),
    }
}

/// A string-content user record: a `!` command, its output, or a message.
fn user_string_entry(
    record: &Value,
    uuid: &str,
    text: &str,
    known: Known<'_>,
) -> Option<ChatEntry> {
    let trimmed = text.trim_start();
    if trimmed.starts_with("<bash-input>") {
        let command = tag_text(text, "bash-input")?.trim();
        return Some(system_row(uuid, format!("! {command}")));
    }
    if trimmed.starts_with("<bash-stdout>") || trimmed.starts_with("<bash-stderr>") {
        return bash_output_row(record, text, known);
    }
    user_message(uuid, text)
}

/// A `!` command's output appended to its command's row, which the record's
/// `parentUuid` names; nothing when that row is unknown or the output empty.
fn bash_output_row(record: &Value, text: &str, known: Known<'_>) -> Option<ChatEntry> {
    let parent = str_at(record, "parentUuid")?;
    let Some(ChatEntry::System { id, text: command }) = known(parent) else {
        return None;
    };
    let output: Vec<String> = ["bash-stdout", "bash-stderr"]
        .into_iter()
        .filter_map(|tag| tag_text(text, tag))
        .map(|part| strip_sgr(part).trim_end().to_owned())
        .filter(|part| !part.trim().is_empty())
        .collect();
    if output.is_empty() {
        return None;
    }
    let output = cap_output(&output.join("\n"), TOOL_RESULT_CAP);
    Some(system_row(&id, format!("{command}\n{output}")))
}

/// A user's typed text as its entry: an interrupt notice is the system row
/// "Interrupted", command noise and blank text are nothing.
fn user_message(uuid: &str, text: &str) -> Option<ChatEntry> {
    if is_interrupt_text(text) {
        return Some(system_row(uuid, "Interrupted".to_owned()));
    }
    if is_command_noise(text) {
        return None;
    }
    let unwrapped = unwrap_pasted(text);
    let text = if unwrapped == text {
        unwrapped
    } else {
        trim_blank_lines(&unwrapped)
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(ChatEntry::User {
        id: uuid.to_owned(),
        text,
        pending: false,
    })
}

/// `text` without the blank lines at its start and end, the ones inside it
/// kept.
fn trim_blank_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(first) = lines.iter().position(|line| !line.trim().is_empty()) else {
        return String::new();
    };
    let last = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .unwrap_or(first);
    lines[first..=last].join("\n")
}

/// A block-content user record: each tool result upserts its card (with any
/// receipt it carries), then the record's text blocks make one message.
fn user_block_entries(
    record: &Value,
    uuid: &str,
    blocks: &[Value],
    known: Known<'_>,
) -> Vec<ChatEntry> {
    let mut entries = Vec::new();
    let mut texts = Vec::new();
    for block in blocks {
        match str_at(block, "type") {
            Some("tool_result") => entries.extend(tool_result_entries(record, block, known)),
            Some("text") => texts.extend(str_at(block, "text")),
            _ => {}
        }
    }
    if !texts.is_empty() {
        entries.extend(user_message(uuid, &texts.join("\n")));
    }
    entries
}

fn tool_result_entries(record: &Value, block: &Value, known: Known<'_>) -> Vec<ChatEntry> {
    let Some(tool_use_id) = str_at(block, "tool_use_id") else {
        return Vec::new();
    };
    let Some(ChatEntry::Tool {
        id,
        name,
        summary,
        input,
        parent_id,
        tail_paths,
        kind,
        ..
    }) = known(tool_use_id)
    else {
        return Vec::new();
    };
    let is_error = flag(block, "is_error");
    let text = block
        .get("content")
        .and_then(content_text)
        .unwrap_or_default();
    let receipts = if is_error {
        Vec::new()
    } else {
        receipts(&name, &id, record.get("toolUseResult"))
    };
    let card = ChatEntry::Tool {
        id,
        name,
        summary,
        input,
        result: Some(ToolOutcome {
            text: cap_output(&text, TOOL_RESULT_CAP),
            is_error,
        }),
        parent_id,
        tail_paths,
        kind,
    };
    std::iter::once(card).chain(receipts).collect()
}

/// The receipts a prompt tool's answered result keeps: one per
/// `AskUserQuestion` question, or the plan approval.
fn receipts(tool: &str, tool_use_id: &str, result: Option<&Value>) -> Vec<ChatEntry> {
    let Some(result) = result else {
        return Vec::new();
    };
    match tool {
        "AskUserQuestion" => result
            .get("answers")
            .and_then(Value::as_object)
            .map(|answers| question_receipts(tool_use_id, result, answers))
            .unwrap_or_default(),
        "ExitPlanMode" if result.get("plan").is_some() => vec![ChatEntry::Receipt {
            id: format!("receipt:{tool_use_id}"),
            question: PLAN_QUESTION.to_owned(),
            answer: PLAN_ANSWER.to_owned(),
        }],
        _ => Vec::new(),
    }
}

/// The answers in the order the questions were asked, then any answer whose
/// question the result does not list. The first is `receipt:<id>`, the
/// next `receipt:<id>:1` and so on.
fn question_receipts(
    tool_use_id: &str,
    result: &Value,
    answers: &Map<String, Value>,
) -> Vec<ChatEntry> {
    let asked: Vec<&str> = result
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|question| str_at(question, "question"))
        .filter(|question| answers.contains_key(*question))
        .collect();
    let unlisted = answers
        .keys()
        .map(String::as_str)
        .filter(|key| !asked.contains(key));
    let ordered: Vec<&str> = asked.iter().copied().chain(unlisted).collect();
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, question)| ChatEntry::Receipt {
            id: if index == 0 {
                format!("receipt:{tool_use_id}")
            } else {
                format!("receipt:{tool_use_id}:{index}")
            },
            question: question.to_owned(),
            answer: answer_text(&answers[question]),
        })
        .collect()
}

/// An answer as text: a multi-select list joined with `, `.
fn answer_text(answer: &Value) -> String {
    match answer {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map_or_else(|| item.to_string(), str::to_owned)
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// A content value's text: the string itself, or its text blocks joined by
/// newlines. Anything else has none.
fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter(|block| str_at(block, "type") == Some("text"))
                .filter_map(|block| str_at(block, "text"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// An assistant record's blocks, one entry each in block order: a run of
/// text blocks as one answer, a signed thinking block as a thinking row,
/// each tool call as a card. The record's first entry takes its `uuid` and
/// each later one `<uuid>:<block index>`; a card keeps its call's id.
fn assistant_entries(record: &Value, uuid: &str) -> Vec<ChatEntry> {
    let Some(blocks) = record.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    let mut in_text_run = false;
    for (index, block) in blocks.iter().enumerate() {
        let id = if entries.is_empty() {
            uuid.to_owned()
        } else {
            format!("{uuid}:{index}")
        };
        let kind = str_at(block, "type");
        match kind {
            Some("text") => {
                let text = str_at(block, "text").unwrap_or("");
                push_answer(&mut entries, id, text, in_text_run);
            }
            Some("thinking") => entries.extend(thinking_row(record, block, id)),
            Some("tool_use") => entries.extend(tool_card(block, str_at(record, "cwd"))),
            _ => {}
        }
        in_text_run = kind == Some("text");
    }
    entries
}

/// Adds `text` to the answer the run of text blocks before it started
/// (`joins`), else starts an answer under `id`. Blank text adds nothing.
fn push_answer(entries: &mut Vec<ChatEntry>, id: String, text: &str, joins: bool) {
    if text.trim().is_empty() {
        return;
    }
    if joins && let Some(ChatEntry::Answer { text: held, .. }) = entries.last_mut() {
        held.push('\n');
        held.push_str(text);
        return;
    }
    entries.push(ChatEntry::Answer {
        id,
        text: text.to_owned(),
        live: false,
    });
}

/// A signed thinking block's row, timed from the record's
/// `thinkingDurationMs` rounded to the nearest second. Unsigned thinking is
/// dropped.
fn thinking_row(record: &Value, block: &Value, id: String) -> Option<ChatEntry> {
    str_at(block, "signature").filter(|signature| !signature.is_empty())?;
    let secs = record
        .get("thinkingDurationMs")
        .and_then(Value::as_u64)
        .and_then(|ms| u32::try_from(ms.saturating_add(500) / 1000).ok());
    Some(ChatEntry::Thinking { id, secs })
}

/// A tool call's card. A shell call names the files its command writes or
/// logs to, resolved against `cwd`, the record's working folder.
fn tool_card(block: &Value, cwd: Option<&str>) -> Option<ChatEntry> {
    let id = str_at(block, "id")?;
    let name = str_at(block, "name")?;
    let input = block.get("input").unwrap_or(&Value::Null);
    let tail_paths = match (Shell::of_tool(name), str_at(input, "command")) {
        (Some(shell), Some(command)) => tail_paths(shell, command, cwd),
        _ => Vec::new(),
    };
    Some(ChatEntry::Tool {
        id: id.to_owned(),
        name: name.to_owned(),
        summary: tool_summary(name, input),
        input: cap(&format!("{input:#}"), TOOL_INPUT_CAP),
        result: None,
        parent_id: None,
        tail_paths,
        kind: tool_kind(name),
    })
}

/// What sort of work Claude's tool `name` does: reading, searching, running
/// a command, or anything else (an edit, a write, a subagent task, an MCP
/// tool).
pub(crate) fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Read" | "NotebookRead" | "WebFetch" => ToolKind::Read,
        "Grep" | "Glob" | "LS" | "WebSearch" | "ToolSearch" => ToolKind::Search,
        _ if Shell::of_tool(name).is_some() || name == "BashOutput" => ToolKind::Run,
        _ => ToolKind::Other,
    }
}

/// One line saying what a call works on: Bash's command, the path for
/// Read/Edit/Write, the pattern for Grep/Glob, the description for Agent and
/// Task; a call with nothing to summarise has none.
fn tool_summary(name: &str, input: &Value) -> String {
    let key = match name {
        "Bash" | "PowerShell" => "command",
        "Read" | "Edit" | "Write" => "file_path",
        "Grep" | "Glob" => "pattern",
        "Agent" | "Task" => "description",
        _ => "",
    };
    let line = str_at(input, key)
        .and_then(|value| value.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or("");
    cap_chars(line, SUMMARY_MAX_CHARS)
}

/// A mid-turn message the user typed (`queued_command`); a queued
/// notification is skipped, as it arrives again as its own record.
fn queued_entry(record: &Value, uuid: &str) -> Option<ChatEntry> {
    let attachment = record.get("attachment")?;
    if str_at(attachment, "type") != Some("queued_command")
        || str_at(attachment, "commandMode") == Some("task-notification")
    {
        return None;
    }
    let prompt = content_text(attachment.get("prompt")?)?;
    user_message(uuid, &prompt)
}

fn system_entry(record: &Value, uuid: &str) -> Option<ChatEntry> {
    match str_at(record, "subtype")? {
        "turn_duration" => Some(ChatEntry::TurnEnd {
            id: uuid.to_owned(),
            duration_ms: Some(record.get("durationMs")?.as_u64()?),
        }),
        "compact_boundary" => Some(system_row(uuid, "Conversation compacted".to_owned())),
        "local_command" => {
            let content = str_at(record, "content")?;
            ["local-command-stdout", "local-command-stderr"]
                .into_iter()
                .filter_map(|tag| tag_text(content, tag))
                .map(|output| strip_sgr(output).trim().to_owned())
                .find(|output| !output.is_empty())
                .map(|output| system_row(uuid, output))
        }
        _ => None,
    }
}

/// A `<task-notification>`'s row. A summary that already reads as a
/// sentence (an agent's or a shell's), and a monitor event's, is shown as
/// written with its first quoted name in backticks; any other summary is the
/// name in a sentence built from the status, an agent told by its result.
fn task_notification_row(text: &str) -> Option<String> {
    let outer = without_tag(text, "result");
    let summary = tag_text(&outer, "summary")?.trim();
    if summary.is_empty() {
        return None;
    }
    let is_event = tag_text(&outer, "event").is_some();
    if is_event
        || SENTENCE_PREFIXES
            .iter()
            .any(|prefix| summary.starts_with(prefix))
    {
        return Some(backtick_first_quotes(summary));
    }
    let is_agent = tag_text(text, "result").is_some();
    let status = tag_text(&outer, "status").map(str::trim);
    let word = match status {
        Some(word @ ("failed" | "killed" | "stopped")) => word,
        _ if is_agent => "finished",
        _ => "completed",
    };
    let kind = if is_agent {
        "Agent"
    } else {
        "Background command"
    };
    Some(format!("{kind} `{summary}` {word}"))
}

/// `text` with its first `"…"` pair turned into backticks.
fn backtick_first_quotes(text: &str) -> String {
    let Some(open) = text.find('"') else {
        return text.to_owned();
    };
    let Some(close) = text[open + 1..].find('"').map(|at| open + 1 + at) else {
        return text.to_owned();
    };
    format!(
        "{}`{}`{}",
        &text[..open],
        &text[open + 1..close],
        &text[close + 1..]
    )
}

/// The text between the first `<tag>` and the `</tag>` after it.
fn tag_text<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

/// `text` without its first `<tag>…</tag>` section.
fn without_tag(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = text.find(&open) else {
        return text.to_owned();
    };
    let Some(end) = text[start..]
        .find(&close)
        .map(|at| start + at + close.len())
    else {
        return text.to_owned();
    };
    format!("{}{}", &text[..start], &text[end..])
}

/// The tag Claude Code wraps a pasted block in.
const PASTED_OPEN: &str = "<pasted_content";
/// The start of the tag's closer; the real one carries an id
/// (`</pasted_content id="23">`), so the tag alone is not enough.
const PASTED_CLOSE: &str = "</pasted_content";

/// Each `<pasted_content …>text</pasted_content …>` replaced by its text; an
/// unclosed one is left as written.
fn unwrap_pasted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(PASTED_OPEN) {
        let after = &rest[start..];
        let Some(body_start) = after.find('>').map(|at| at + 1) else {
            break;
        };
        let body = &after[body_start..];
        let Some(close) = closer_start(body) else {
            break;
        };
        let closer = &body[close..];
        let Some(closer_end) = closer.find('>') else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str(&body[..close]);
        rest = &closer[closer_end + 1..];
    }
    out.push_str(rest);
    out
}

/// Where the closer of the paste at the start of `text` begins: the end tag
/// followed by `>` or a space, then its attributes up to a `>`. Text that
/// merely names the tag, such as a string holding `"</pasted_content"`,
/// does not close the paste.
fn closer_start(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = text[from..].find(PASTED_CLOSE) {
        let start = from + at;
        let after = text[start + PASTED_CLOSE.len()..].chars().next();
        if matches!(after, Some('>' | ' ')) {
            return Some(start);
        }
        from = start + PASTED_CLOSE.len();
    }
    None
}

/// `text` without its SGR escapes (`ESC [ <params> m`); other escapes stay.
fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\u{1b}') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let params = after.strip_prefix('[').map(|tail| {
            tail.trim_start_matches(|c: char| c.is_ascii_digit() || c == ';' || c == ':')
        });
        if let Some(tail) = params.and_then(|tail| tail.strip_prefix('m')) {
            rest = tail;
        } else {
            out.push('\u{1b}');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `text` split at the last char boundary at or below `limit`: the prefix
/// kept and the rest dropped.
fn split_at_limit(text: &str, limit: usize) -> (&str, &str) {
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.split_at(end)
}

/// `text` cut to at most `limit` bytes on a char boundary, with
/// [`CUT_MARKER`] after the kept part when anything was cut.
fn cap(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let (kept, _) = split_at_limit(text, limit);
    format!("{kept}{CUT_MARKER}")
}

/// `text` without one trailing `\r\n` or `\n`.
fn without_trailing_newline(text: &str) -> &str {
    text.strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text)
}

/// `text` cut to at most `limit` bytes on a char boundary like [`cap`], then
/// a line saying how much the cut dropped: the lines left out, the partly
/// kept one counting as one, or the characters when the text is one line.
fn cap_output(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let (kept, dropped) = split_at_limit(text, limit);
    let cut = if without_trailing_newline(text).contains('\n') {
        let rest = if kept.ends_with('\n') {
            dropped
        } else {
            dropped.strip_prefix('\n').unwrap_or(dropped)
        };
        let lines = rest.lines().count();
        let noun = if lines == 1 { "line" } else { "lines" };
        format!("{CUT_MARKER} {lines} more {noun}")
    } else {
        let chars = without_trailing_newline(dropped).chars().count();
        let noun = if chars == 1 {
            "character"
        } else {
            "characters"
        };
        format!("{CUT_MARKER} {chars} more {noun}")
    };
    let separator = if kept.ends_with('\n') { "" } else { "\n" };
    format!("{kept}{separator}{cut}")
}

/// `text` cut to at most `max` characters, the last being [`CUT_MARKER`]
/// when anything was cut.
fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push(CUT_MARKER);
    cut
}

#[cfg(test)]
mod tests {
    use super::{
        CUT_MARKER, TOOL_INPUT_CAP, TOOL_RESULT_CAP, cap, cap_output, chat_entries, line_facts,
        split_at_limit, tool_kind,
    };
    use agent_api::TranscriptLineFacts;
    use chrono::{TimeDelta, TimeZone as _, Utc};
    use protocol::{ChatEntry, ToolKind, ToolOutcome};
    use serde_json::{Value, json};

    #[test]
    fn a_typed_prompt_carries_its_prompt_id() {
        let line = json!({
            "type": "user", "uuid": uid("01"), "promptId": "p-1",
            "timestamp": "2026-09-27T10:00:01.500Z",
            "message": {"role": "user", "content": "Hello there"},
        })
        .to_string();
        assert_eq!(
            line_facts(&line),
            TranscriptLineFacts {
                prompt_id: Some("p-1".to_owned()),
                is_user_prompt: true,
                is_interrupt: false,
                timestamp: Utc
                    .with_ymd_and_hms(2026, 9, 27, 10, 0, 1)
                    .single()
                    .map(|second| second + TimeDelta::milliseconds(500)),
                ended_tasks: Vec::new(),
            }
        );
    }

    #[test]
    fn line_facts_list_the_task_a_notification_ends() {
        assert_eq!(line_facts(line("notifications", "31")).ended_tasks, ["b1"]);
        assert_eq!(line_facts(line("notifications", "30")).ended_tasks, ["a1"]);
        assert_eq!(
            line_facts(line("notifications", "32")).ended_tasks,
            Vec::<String>::new(),
            "a monitor's event leaves its monitor running"
        );
        assert_eq!(
            line_facts(line("notifications", "3b")).ended_tasks,
            ["a6"],
            "a summary inside the result is not the notification's"
        );
    }

    #[test]
    fn a_monitor_event_beside_a_shells_end_lists_the_shell() {
        let record = json!({
            "type": "user",
            "uuid": uid("40"),
            "timestamp": "2026-09-27T10:00:51.000Z",
            "message": {
                "role": "user",
                "content": "<task-notification><task-id>m1</task-id>\
                    <summary>Monitor \"build log\" matched a line</summary>\
                    <event>error: x</event></task-notification>\n\
                    <task-notification><task-id>b1</task-id><tool-use-id>toolu_01</tool-use-id>\
                    <output-file>o</output-file><status>completed</status>\
                    <summary>Background command \"Run tests\" completed (exit code 0)</summary>\
                    </task-notification>",
            },
            "origin": {"kind": "task-notification"},
        });
        assert_eq!(
            line_facts(&record.to_string()).ended_tasks,
            ["b1"],
            "the monitor runs on; the shell ended"
        );
    }

    #[test]
    fn a_line_carries_its_records_time_and_none_for_a_missing_or_bad_one() {
        let stamped = json!({
            "type": "assistant", "uuid": uid("06"), "timestamp": "2026-09-27T10:00:02+02:00",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        });
        assert_eq!(
            line_facts(&stamped.to_string()).timestamp,
            Utc.with_ymd_and_hms(2026, 9, 27, 8, 0, 2).single()
        );
        let missing = json!({
            "type": "assistant", "uuid": uid("07"),
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        });
        let bad = json!({
            "type": "assistant", "uuid": uid("08"), "timestamp": "yesterday",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        });
        let number = json!({
            "type": "assistant", "uuid": uid("09"), "timestamp": 1_790_000_000,
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        });
        for record in [missing, bad, number] {
            assert_eq!(line_facts(&record.to_string()).timestamp, None, "{record}");
        }
    }

    #[test]
    fn only_a_user_record_with_text_is_a_prompt() {
        let text_beside_result = json!({
            "type": "user", "uuid": uid("02"), "promptId": "p-1",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
                {"type": "text", "text": "and also this"},
            ]},
        });
        assert!(line_facts(&text_beside_result.to_string()).is_user_prompt);
        let not_prompts = [
            json!({"type": "user", "uuid": uid("03"), "promptId": "p-1",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}}),
            json!({"type": "user", "uuid": uid("04"), "isMeta": true,
                "message": {"role": "user", "content": "caveat"}}),
            json!({"type": "assistant", "uuid": uid("05"), "promptId": "p-1",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]}}),
        ];
        for record in not_prompts {
            let facts = line_facts(&record.to_string());
            assert!(!facts.is_user_prompt, "{record}");
        }
    }

    #[test]
    fn an_interrupt_notice_is_not_a_prompt() {
        let interrupts = [
            json!({"type": "user", "uuid": uid("11"),
                "message": {"role": "user", "content": "[Request interrupted by user]"}}),
            json!({"type": "user", "uuid": uid("12"),
                "message": {"role": "user", "content": [
                    {"type": "text", "text": "[Request interrupted by user for tool use]"}]}}),
        ];
        for record in interrupts {
            let facts = line_facts(&record.to_string());
            assert!(!facts.is_user_prompt, "{record}");
            assert!(facts.is_interrupt, "{record}");
        }
    }

    #[test]
    fn a_line_that_does_not_parse_has_no_facts() {
        assert_eq!(line_facts("{not json"), TranscriptLineFacts::default());
    }

    fn uid(n: &str) -> String {
        format!("00000000-0000-4000-8000-0000000000{n}")
    }

    fn id_of(entry: &ChatEntry) -> &str {
        match entry {
            ChatEntry::User { id, .. }
            | ChatEntry::Answer { id, .. }
            | ChatEntry::Thinking { id, .. }
            | ChatEntry::Tool { id, .. }
            | ChatEntry::System { id, .. }
            | ChatEntry::Receipt { id, .. }
            | ChatEntry::TurnEnd { id, .. } => id,
            ChatEntry::Unknown => "",
        }
    }

    /// Every line of `body` through [`chat_entries`], each output upserted by
    /// id into the chat the next line's `known` reads: the per-line outputs
    /// and the chat at the end.
    fn replay(body: &str) -> (Vec<Vec<ChatEntry>>, Vec<ChatEntry>) {
        let mut chat: Vec<ChatEntry> = Vec::new();
        let mut outputs = Vec::new();
        for line in body.lines() {
            let out = {
                let held = &chat;
                chat_entries(line, &|id| held.iter().find(|e| id_of(e) == id).cloned())
            };
            for entry in &out {
                match chat.iter_mut().find(|held| id_of(held) == id_of(entry)) {
                    Some(held) => *held = entry.clone(),
                    None => chat.push(entry.clone()),
                }
            }
            outputs.push(out);
        }
        (outputs, chat)
    }

    fn user(n: &str, text: &str) -> ChatEntry {
        ChatEntry::User {
            id: uid(n),
            text: text.to_owned(),
            pending: false,
        }
    }

    fn answer(n: &str, text: &str) -> ChatEntry {
        ChatEntry::Answer {
            id: uid(n),
            text: text.to_owned(),
            live: false,
        }
    }

    fn system(n: &str, text: &str) -> ChatEntry {
        ChatEntry::System {
            id: uid(n),
            text: text.to_owned(),
        }
    }

    fn receipt(id: &str, question: &str, answer: &str) -> ChatEntry {
        ChatEntry::Receipt {
            id: id.to_owned(),
            question: question.to_owned(),
            answer: answer.to_owned(),
        }
    }

    fn tool(
        id: &str,
        name: &str,
        summary: &str,
        input: &Value,
        result: Option<(&str, bool)>,
    ) -> ChatEntry {
        ChatEntry::Tool {
            id: id.to_owned(),
            name: name.to_owned(),
            summary: summary.to_owned(),
            input: format!("{input:#}"),
            result: result.map(|(text, is_error)| ToolOutcome {
                text: text.to_owned(),
                is_error,
            }),
            parent_id: None,
            tail_paths: Vec::new(),
            kind: tool_kind(name),
        }
    }

    #[test]
    fn each_tool_name_has_its_kind_and_any_other_is_other() {
        for (names, kind) in [
            (&["Read", "NotebookRead", "WebFetch"][..], ToolKind::Read),
            (
                &["Grep", "Glob", "LS", "WebSearch", "ToolSearch"][..],
                ToolKind::Search,
            ),
            (&["Bash", "PowerShell", "BashOutput"][..], ToolKind::Run),
            (
                &["Edit", "Write", "Task", "Agent", "mcp__godot__add_node"][..],
                ToolKind::Other,
            ),
        ] {
            for name in names {
                assert_eq!(tool_kind(name), kind, "{name}");
            }
        }
    }

    fn fixture(name: &str) -> &'static str {
        match name {
            "plain-turn" => include_str!("../tests/fixtures/transcripts/plain-turn.jsonl"),
            "tools" => include_str!("../tests/fixtures/transcripts/tools.jsonl"),
            "queued" => include_str!("../tests/fixtures/transcripts/queued.jsonl"),
            "notifications" => include_str!("../tests/fixtures/transcripts/notifications.jsonl"),
            "bang-and-paste" => include_str!("../tests/fixtures/transcripts/bang-and-paste.jsonl"),
            "ask-user-question" => {
                include_str!("../tests/fixtures/transcripts/ask-user-question.jsonl")
            }
            "plan-approval" => include_str!("../tests/fixtures/transcripts/plan-approval.jsonl"),
            "compact" => include_str!("../tests/fixtures/transcripts/compact.jsonl"),
            "thinking" => include_str!("../tests/fixtures/transcripts/thinking.jsonl"),
            "noise" => include_str!("../tests/fixtures/transcripts/noise.jsonl"),
            "permission-amend-deny" => {
                include_str!("../tests/fixtures/transcripts/permission-amend-deny.jsonl")
            }
            "caps" => include_str!("../tests/fixtures/transcripts/caps.jsonl"),
            other => unreachable!("no fixture {other}"),
        }
    }

    fn line(name: &str, n: &str) -> &'static str {
        let needle = format!("\"uuid\":\"{}\"", uid(n));
        fixture(name)
            .lines()
            .find(|line| line.contains(&needle))
            .unwrap_or_else(|| unreachable!("{name} has no record {n}"))
    }

    fn alone(name: &str, n: &str) -> Vec<ChatEntry> {
        chat_entries(line(name, n), &|_| None)
    }

    #[test]
    fn user_text_and_answers_make_one_entry_per_record_in_order() {
        let (outputs, chat) = replay(fixture("plain-turn"));

        assert!(outputs.iter().all(|out| out.len() == 1), "{outputs:?}");
        assert_eq!(
            chat,
            [
                user("01", "Hello there"),
                user("02", "Look at\nthis file"),
                answer("03", "Hi! How can I help?"),
                answer("04", "Here is the second part."),
                ChatEntry::TurnEnd {
                    id: uid("05"),
                    duration_ms: Some(4200),
                },
            ]
        );
    }

    #[test]
    fn tool_results_land_on_their_cards() {
        let (outputs, chat) = replay(fixture("tools"));

        assert_eq!(
            chat,
            [
                tool(
                    "toolu_01",
                    "Bash",
                    "cargo test",
                    &json!({"command": "cargo test\n--all", "description": "Run tests"}),
                    Some(("ok", false)),
                ),
                tool(
                    "toolu_02",
                    "Read",
                    r"D:\proj\src\lib.rs",
                    &json!({"file_path": r"D:\proj\src\lib.rs"}),
                    Some(("line one\nline two", false)),
                ),
                tool(
                    "toolu_03",
                    "Grep",
                    "fn main",
                    &json!({"pattern": "fn main", "path": "src"}),
                    Some(("boom", true)),
                ),
                tool(
                    "toolu_04",
                    "Agent",
                    "Explore the map",
                    &json!({"description": "Explore the map", "prompt": "Look around",
                            "subagent_type": "general-purpose"}),
                    None,
                ),
                tool(
                    "toolu_05",
                    "WebFetch",
                    "",
                    &json!({"url": "https://example.test/"}),
                    None
                ),
            ]
        );
        assert_eq!(outputs[1].len(), 1, "a result answers with its card only");
        assert_eq!(
            outputs[8],
            [],
            "a result whose card is unknown adds nothing"
        );
    }

    fn summary_of(name: &str, input: &Value) -> String {
        let line = json!({"type": "assistant", "uuid": "u", "message": {"content": [
            {"type": "tool_use", "id": "toolu_x", "name": name, "input": input}
        ]}})
        .to_string();
        match chat_entries(&line, &|_| None).as_slice() {
            [ChatEntry::Tool { summary, .. }] => summary.clone(),
            other => unreachable!("one card expected, got {other:?}"),
        }
    }

    #[test]
    fn tool_summary_names_what_the_call_works_on() {
        assert_eq!(summary_of("Edit", &json!({"file_path": "a.rs"})), "a.rs");
        assert_eq!(summary_of("Write", &json!({"file_path": "b.rs"})), "b.rs");
        assert_eq!(
            summary_of("Glob", &json!({"pattern": "**/*.rs"})),
            "**/*.rs"
        );
        assert_eq!(summary_of("Task", &json!({"description": "Look"})), "Look");
        assert_eq!(
            summary_of("Bash", &json!({"command": "  \n  ls -la\nrm x"})),
            "ls -la"
        );
        assert_eq!(summary_of("Bash", &json!({"command": ""})), "");
        assert_eq!(summary_of("Read", &json!({})), "");
        assert_eq!(summary_of("Bash", &json!({"command": 7})), "");
        assert_eq!(summary_of("ExitPlanMode", &json!({"plan": "x"})), "");
        assert_eq!(
            summary_of("PowerShell", &json!({"command": "Get-ChildItem"})),
            "Get-ChildItem"
        );
    }

    #[test]
    fn a_shell_card_names_the_files_its_command_writes_and_keeps_them_with_its_result() {
        let cwd = std::env::temp_dir().join("repo");
        let line = json!({"type": "assistant", "uuid": "u", "cwd": cwd.display().to_string(),
        "message": {"content": [
            {"type": "tool_use", "id": "toolu_t", "name": "Bash",
                "input": {"command": "make 2>&1 | tee make.log"}},
            {"type": "tool_use", "id": "toolu_r", "name": "Read",
                "input": {"file_path": "make.log", "command": "> x.log"}}
        ]}})
        .to_string();
        let cards = chat_entries(&line, &|_| None);
        let expected = vec![cwd.join("make.log").display().to_string()];
        let [
            ChatEntry::Tool { tail_paths, .. },
            ChatEntry::Tool {
                tail_paths: none, ..
            },
        ] = cards.as_slice()
        else {
            unreachable!("two cards expected, got {cards:?}");
        };
        assert_eq!(tail_paths, &expected);
        assert_eq!(
            *none,
            Vec::<String>::new(),
            "a tool that runs no shell names no file"
        );

        let result = json!({"type": "user", "uuid": "v", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "toolu_t", "content": "ok"}
        ]}})
        .to_string();
        let first = cards[0].clone();
        let known = move |id: &str| (id == "toolu_t").then(|| first.clone());
        let updated = chat_entries(&result, &known);
        assert!(
            matches!(updated.as_slice(), [ChatEntry::Tool { tail_paths, result: Some(_), .. }] if *tail_paths == expected),
            "{updated:?}"
        );
    }

    #[test]
    fn a_result_keeps_the_kind_its_card_was_given() {
        let card = ChatEntry::Tool {
            id: "toolu_k".to_owned(),
            name: "Read".to_owned(),
            summary: "a.rs".to_owned(),
            input: "{}".to_owned(),
            result: None,
            parent_id: None,
            tail_paths: Vec::new(),
            kind: ToolKind::Search,
        };
        let result = json!({"type": "user", "uuid": "v", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "toolu_k", "content": "ok"}
        ]}})
        .to_string();
        let known = move |id: &str| (id == "toolu_k").then(|| card.clone());
        let updated = chat_entries(&result, &known);
        assert!(
            matches!(
                updated.as_slice(),
                [ChatEntry::Tool {
                    kind: ToolKind::Search,
                    result: Some(_),
                    ..
                }]
            ),
            "{updated:?}"
        );
    }

    #[test]
    fn queued_messages_are_user_entries_and_queued_notifications_are_skipped() {
        let (_, chat) = replay(fixture("queued"));

        assert_eq!(chat, [user("20", "also run clippy")]);
    }

    #[test]
    fn task_notifications_are_system_rows() {
        let expected = [
            ("30", "Agent `Explore the map` finished"),
            (
                "31",
                "Background command `Run tests` completed (exit code 0)",
            ),
            ("32", "Monitor `build log` matched a line"),
            ("33", "Agent `Explore` finished"),
            ("34", "Agent `Explore` killed"),
            ("35", "Agent `Explore` failed"),
            ("36", "Agent `Explore` stopped"),
            ("37", "Background command `npm run dev` completed"),
            ("38", "Background command `npm run dev` failed"),
            ("39", "Background command `npm run dev` killed"),
            ("3a", "Background command `npm run dev` stopped"),
            ("3b", "Agent `Real` finished"),
        ];
        for (n, text) in expected {
            assert_eq!(alone("notifications", n), [system(n, text)], "record {n}");
        }
        let (_, chat) = replay(fixture("notifications"));
        assert_eq!(chat.len(), expected.len());
    }

    #[test]
    fn bang_command_and_its_output_fold_into_one_system_row() {
        let (outputs, chat) = replay(fixture("bang-and-paste"));

        assert_eq!(outputs[0], [system("40", "! git status")]);
        assert_eq!(
            outputs[1],
            [system(
                "40",
                "! git status\nOn branch main\nnothing to commit"
            )]
        );
        assert_eq!(outputs[2], [], "output whose command is unknown");
        assert_eq!(outputs[7], [], "a command with no output changes nothing");
        assert_eq!(
            chat,
            [
                system("40", "! git status\nOn branch main\nnothing to commit"),
                system("43", "! bad-cmd\nnot found"),
                user("45", "Please read line a\nline b and fix it"),
                system("46", "! true"),
            ]
        );
    }

    #[test]
    fn pasted_content_is_unwrapped() {
        assert_eq!(
            alone("bang-and-paste", "45"),
            [user("45", "Please read line a\nline b and fix it")]
        );
        let unclosed = json!({"type": "user", "uuid": "u",
            "message": {"content": "a <pasted_content id=\"1\">b"}})
        .to_string();
        assert_eq!(
            chat_entries(&unclosed, &|_| None),
            [ChatEntry::User {
                id: "u".to_owned(),
                text: "a <pasted_content id=\"1\">b".to_owned(),
                pending: false,
            }]
        );
    }

    #[test]
    fn a_paste_closed_with_its_id_is_unwrapped() {
        let closed = json!({"type": "user", "uuid": "u",
            "message": {"content": "\n\n<pasted_content id=\"23ae\">line a\nline b</pasted_content id=\"23ae\">\nfix it"}})
        .to_string();
        assert_eq!(
            chat_entries(&closed, &|_| None),
            [ChatEntry::User {
                id: "u".to_owned(),
                text: "line a\nline b\nfix it".to_owned(),
                pending: false,
            }]
        );
        let plain = json!({"type": "user", "uuid": "u",
            "message": {"content": "\n\nhello\n\n"}})
        .to_string();
        assert_eq!(
            chat_entries(&plain, &|_| None),
            [ChatEntry::User {
                id: "u".to_owned(),
                text: "\n\nhello\n\n".to_owned(),
                pending: false,
            }]
        );
    }

    #[test]
    fn a_closer_named_inside_a_paste_does_not_close_it() {
        let body = "const CLOSE: &str = \"</pasted_content\";\nfn f() -> u8 {}";
        let content = format!("<pasted_content id=\"1\">{body}</pasted_content id=\"1\">");
        let record =
            json!({"type": "user", "uuid": "u", "message": {"content": content}}).to_string();
        assert_eq!(
            chat_entries(&record, &|_| None),
            [ChatEntry::User {
                id: "u".to_owned(),
                text: body.to_owned(),
                pending: false,
            }]
        );
    }

    #[test]
    fn a_blank_paste_makes_no_entry() {
        let record = json!({"type": "user", "uuid": "u",
            "message": {"content": "<pasted_content id=\"1\">\n  \n</pasted_content id=\"1\">"}})
        .to_string();
        assert_eq!(chat_entries(&record, &|_| None), []);
    }

    #[test]
    fn slash_command_output_is_a_system_row_without_sgr_escapes() {
        assert_eq!(alone("compact", "72"), [system("72", "Model set to fake")]);
        assert_eq!(alone("compact", "73"), [], "the command line itself");
        assert_eq!(alone("compact", "74"), [], "empty output");
    }

    #[test]
    fn compact_boundary_is_a_system_row_and_the_summary_is_dropped() {
        assert_eq!(
            alone("compact", "70"),
            [system("70", "Conversation compacted")]
        );
        assert_eq!(alone("compact", "71"), []);
    }

    #[test]
    fn non_message_records_are_dropped() {
        for line in fixture("noise").lines() {
            assert_eq!(chat_entries(line, &|_| None), [], "{line}");
        }
    }

    #[test]
    fn signed_thinking_is_a_row_timed_from_its_duration() {
        let thinking = |n: &str, secs: Option<u32>| ChatEntry::Thinking { id: uid(n), secs };

        assert_eq!(alone("thinking", "80"), [thinking("80", Some(14))]);
        assert_eq!(
            alone("thinking", "81"),
            [thinking("81", None)],
            "no duration field"
        );
        assert_eq!(alone("thinking", "82"), [], "empty signature");
        assert_eq!(alone("thinking", "83"), [], "no signature");
        assert_eq!(alone("thinking", "84"), [], "redacted");
        assert_eq!(
            alone("thinking", "85"),
            [thinking("85", Some(2))],
            "1500 ms rounds up"
        );
        assert_eq!(
            alone("thinking", "86"),
            [thinking("86", Some(0))],
            "499 ms rounds down"
        );
    }

    #[test]
    fn ask_user_question_answers_become_receipts() {
        let (outputs, _) = replay(fixture("ask-user-question"));

        assert!(
            matches!(&outputs[1][0], ChatEntry::Tool { id, result: Some(_), .. } if id == "toolu_10"),
            "{:?}",
            outputs[1]
        );
        assert_eq!(
            outputs[1][1..],
            [
                receipt("receipt:toolu_10", "Which tests?", "Unit"),
                receipt("receipt:toolu_10:1", "Any style?", "Terse, Plain"),
            ]
        );
        assert_eq!(
            outputs[3][1..],
            [receipt("receipt:toolu_11", "Pick?", "A, B")]
        );
    }

    #[test]
    fn an_approved_plan_becomes_a_receipt_and_a_rejected_one_does_not() {
        let (outputs, _) = replay(fixture("plan-approval"));

        assert_eq!(outputs[1].len(), 2, "{:?}", outputs[1]);
        assert_eq!(
            outputs[1][1],
            receipt("receipt:toolu_20", "Ready to code?", "Approved the plan")
        );
        assert!(
            matches!(
                &outputs[3][..],
                [ChatEntry::Tool {
                    result: Some(ToolOutcome { is_error: true, .. }),
                    ..
                }]
            ),
            "{:?}",
            outputs[3]
        );
    }

    #[test]
    fn an_amend_is_a_user_message_and_a_deny_is_an_error_and_interrupted() {
        let (outputs, chat) = replay(fixture("permission-amend-deny"));
        let bash = json!({"command": "python -c \"print(6*7)\""});

        assert_eq!(
            outputs[1],
            [
                tool(
                    "toolu_30",
                    "Bash",
                    "python -c \"print(6*7)\"",
                    &bash,
                    Some(("42", false))
                ),
                user("b1", "Yes, but print 6*8 instead"),
            ]
        );
        assert!(
            matches!(&outputs[3][..], [ChatEntry::Tool { id, result: Some(ToolOutcome { is_error: true, .. }), .. }] if id == "toolu_31"),
            "{:?}",
            outputs[3]
        );
        assert_eq!(outputs[4], [system("b4", "Interrupted")]);
        assert_eq!(outputs[5], [system("b5", "Interrupted")], "Esc mid-turn");
        assert_eq!(outputs[6], [system("b6", "Interrupted")], "string content");
        assert!(
            !chat
                .iter()
                .any(|e| matches!(e, ChatEntry::User { text, .. } if text.contains("interrupted"))),
            "{chat:?}"
        );
    }

    #[test]
    fn tool_text_is_capped_on_a_char_boundary_and_answers_are_not() {
        let (_, chat) = replay(fixture("caps"));
        let full_input = format!("{:#}", json!({"command": "é".repeat(3000)}));

        let ChatEntry::Tool {
            input,
            result: Some(result),
            ..
        } = &chat[0]
        else {
            unreachable!("a card with a result: {:?}", chat[0]);
        };
        assert!(
            input.len() <= TOOL_INPUT_CAP + '…'.len_utf8(),
            "{}",
            input.len()
        );
        let kept = input
            .strip_suffix('…')
            .unwrap_or_else(|| unreachable!("marker"));
        assert!(full_input.starts_with(kept));
        let full = fixture("caps")
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|record| {
                record
                    .pointer("/message/content/0/content")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let Some(full) = full else {
            unreachable!("the caps fixture holds a result text");
        };
        let kept = "日".repeat(TOOL_RESULT_CAP / 3);
        assert_eq!(
            result.text,
            format!(
                "{kept}\n{CUT_MARKER} {} more characters",
                full.chars().count() - kept.chars().count()
            )
        );
        assert!(matches!(&chat[1], ChatEntry::Answer { text, .. } if text.len() == 20000));
        assert!(matches!(&chat[2], ChatEntry::User { text, .. } if text.len() == 20000));
        let ChatEntry::Tool { summary, .. } = &chat[3] else {
            unreachable!("a card: {:?}", chat[3]);
        };
        assert_eq!(summary.chars().count(), 120);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn a_record_with_several_blocks_keeps_their_order_and_distinct_ids() {
        let record = |blocks: Value| {
            json!({"type": "assistant", "uuid": "u", "message": {"content": blocks}}).to_string()
        };
        let answer_as = |id: &str, text: &str| ChatEntry::Answer {
            id: id.to_owned(),
            text: text.to_owned(),
            live: false,
        };
        let thought = record(json!([
            {"type": "thinking", "thinking": "", "signature": "sig"},
            {"type": "text", "text": "hi"}
        ]));
        assert_eq!(
            chat_entries(&thought, &|_| None),
            [
                ChatEntry::Thinking {
                    id: "u".to_owned(),
                    secs: None
                },
                answer_as("u:1", "hi"),
            ]
        );
        let around_a_card = record(json!([
            {"type": "text", "text": "A"},
            {"type": "tool_use", "id": "toolu_t", "name": "Bash", "input": {"command": "ls"}},
            {"type": "text", "text": "B"}
        ]));
        assert_eq!(
            chat_entries(&around_a_card, &|_| None),
            [
                answer_as("u", "A"),
                tool("toolu_t", "Bash", "ls", &json!({"command": "ls"}), None),
                answer_as("u:2", "B"),
            ]
        );
        let run = record(json!([{"type": "text", "text": "A"}, {"type": "text", "text": "B"}]));
        assert_eq!(chat_entries(&run, &|_| None), [answer_as("u", "A\nB")]);
    }

    #[test]
    fn an_empty_stdout_tag_falls_back_to_stderr() {
        let line = json!({"type": "system", "subtype": "local_command", "uuid": "u",
            "content": "<local-command-stdout></local-command-stdout><local-command-stderr>Error: x</local-command-stderr>"})
        .to_string();
        assert_eq!(
            chat_entries(&line, &|_| None),
            [ChatEntry::System {
                id: "u".to_owned(),
                text: "Error: x".to_owned()
            }]
        );
    }

    #[test]
    fn cap_keeps_short_text_and_cuts_inside_no_char() {
        assert_eq!(cap("ab", 2), "ab");
        assert_eq!(cap("abc", 2), "ab…");
        assert_eq!(cap("aé", 2), "a…");
        assert_eq!(cap("", 0), "");
    }

    /// The kept prefix and the dropped rest of `text` at the result cap.
    fn cut(text: &str) -> (&str, &str) {
        split_at_limit(text, TOOL_RESULT_CAP)
    }

    #[test]
    fn a_cut_result_says_how_many_lines_it_dropped() {
        let text: String = (1..=300)
            .map(|n| format!("row {n}: the quick brown fox jumps over the lazy dog\n"))
            .collect::<Vec<String>>()
            .concat();
        let (kept, dropped) = cut(&text);
        assert!(!kept.ends_with('\n'), "the cut lands inside a line");
        let dropped = dropped.lines().count();
        assert_eq!(
            cap_output(&text, TOOL_RESULT_CAP),
            format!("{kept}\n{CUT_MARKER} {dropped} more lines")
        );
    }

    #[test]
    fn a_cut_single_line_result_says_how_many_characters_it_dropped() {
        let text = "é".repeat(5000);
        let (kept, dropped) = cut(&text);
        let dropped = dropped.chars().count();
        assert_eq!(
            cap_output(&text, TOOL_RESULT_CAP),
            format!("{kept}\n{CUT_MARKER} {dropped} more characters")
        );
    }

    #[test]
    fn a_cut_before_a_newline_drops_no_extra_line() {
        let text = format!("{}\nbbb", "a".repeat(TOOL_RESULT_CAP));
        let (kept, dropped) = cut(&text);
        assert_eq!(dropped, "\nbbb", "the cut lands just before the newline");
        assert_eq!(
            cap_output(&text, TOOL_RESULT_CAP),
            format!("{kept}\n{CUT_MARKER} 1 more line")
        );
    }

    #[test]
    fn a_trailing_newline_does_not_make_a_second_line() {
        let text = format!("{}\n", "x".repeat(9000));
        let (kept, _) = cut(&text);
        assert_eq!(
            cap_output(&text, TOOL_RESULT_CAP),
            format!(
                "{kept}\n{CUT_MARKER} {} more characters",
                9000 - TOOL_RESULT_CAP
            )
        );
    }

    #[test]
    fn a_cut_after_a_newline_adds_no_blank_line() {
        let text = format!("{}\n{}", "a".repeat(TOOL_RESULT_CAP - 1), "b".repeat(1000));
        let (kept, _) = cut(&text);
        assert!(kept.ends_with('\n'), "the cut lands just after a newline");
        assert_eq!(
            cap_output(&text, TOOL_RESULT_CAP),
            format!("{kept}{CUT_MARKER} 1 more line")
        );
    }

    #[test]
    fn one_more_line_is_singular() {
        let text = format!("\n{}", "a".repeat(9000));
        let kept = format!("\n{}", "a".repeat(TOOL_RESULT_CAP - 1));
        assert_eq!(
            cap_output(&text, TOOL_RESULT_CAP),
            format!("{kept}\n{CUT_MARKER} 1 more line")
        );
    }

    #[test]
    fn one_more_character_is_singular() {
        let text = "a".repeat(TOOL_RESULT_CAP + 1);
        let kept = "a".repeat(TOOL_RESULT_CAP);
        assert_eq!(
            cap_output(&text, TOOL_RESULT_CAP),
            format!("{kept}\n{CUT_MARKER} 1 more character")
        );
    }

    #[test]
    fn malformed_and_unknown_lines_yield_nothing() {
        let card = tool("toolu_1", "Bash", "ls", &json!({"command": "ls"}), None);
        let lines = [
            "",
            "   ",
            "not json",
            "[1, 2]",
            "null",
            "\"text\"",
            "{\"type\": \"user\"",
            r#"{"type": 7, "uuid": "u"}"#,
            r#"{"type": "weird", "uuid": "u"}"#,
            r#"{"type": "user", "message": {"content": "no uuid"}}"#,
            r#"{"type": "user", "uuid": "", "message": {"content": "empty uuid"}}"#,
            r#"{"type": "user", "uuid": "u"}"#,
            r#"{"type": "user", "uuid": "u", "message": {"content": 5}}"#,
            r#"{"type": "user", "uuid": "u", "message": {"content": "   "}}"#,
            r#"{"type": "assistant", "uuid": "u", "message": {"content": "a string"}}"#,
            r#"{"type": "assistant", "uuid": "u", "message": {"content": [{"type": "tool_use", "name": "Bash"}]}}"#,
            r#"{"type": "assistant", "uuid": "u", "message": {"content": [{"type": "text", "text": ""}]}}"#,
            r#"{"type": "user", "uuid": "u", "message": {"content": [{"type": "tool_result"}]}}"#,
            r#"{"type": "system", "uuid": "u"}"#,
            r#"{"type": "attachment", "uuid": "u"}"#,
            r#"{"type": "attachment", "uuid": "u", "attachment": {"type": "queued_command"}}"#,
        ];
        for line in lines {
            assert_eq!(chat_entries(line, &|_| Some(card.clone())), [], "{line}");
        }
        let not_a_card = r#"{"type": "user", "uuid": "u", "message": {"content": [{"type": "tool_result", "tool_use_id": "x", "content": "r"}]}}"#;
        assert_eq!(
            chat_entries(not_a_card, &|_| Some(system("01", "a row"))),
            [],
            "a result whose id holds no card"
        );
    }
}
