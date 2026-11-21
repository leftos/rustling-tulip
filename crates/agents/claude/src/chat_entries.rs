//! Claude Code transcript records turned into chat entries.
//!
//! Each transcript line is one JSON record. A record's entries keep the
//! record's place in the file; ordering them across records is the mirror's.
//! Ids: a message's entries use the record `uuid`, a tool card the `tool_use`
//! block's id (so its result upserts the card), a receipt `receipt:<that id>`.

use agent_api::{ModelSwitch, TranscriptLineFacts};
use chrono::{DateTime, Utc};
use protocol::{ChatEntry, ScriptFacts, ToolKind, ToolOutcome};
use serde_json::{Map, Value};

use crate::records;
use crate::scripts::{self, Origin, ReplEvent, ScriptBook, Tie};
use crate::tail_paths::{Shell, tail_paths};
use crate::transcripts::{is_command_noise, is_meta};

/// The most bytes of a tool call's input a card keeps.
const TOOL_INPUT_CAP: usize = 4096;
/// The most bytes of a tool call's result, or of a `!` command's output, a
/// row keeps.
const TOOL_RESULT_CAP: usize = 8192;
/// The longest tool card summary, in characters, including the ellipsis.
pub(crate) const SUMMARY_MAX_CHARS: usize = 120;
/// What a cut text ends with.
const CUT_MARKER: char = '…';
/// The prefixes of a notification summary that already reads as a sentence.
const SENTENCE_PREFIXES: [&str; 2] = ["Agent \"", "Background command \""];
/// The question a plan approval's receipt answers: the dialog's title.
const PLAN_QUESTION: &str = "Ready to code?";
const PLAN_ANSWER: &str = "Approved the plan";
/// The `origin.kind` a record carries when a task notification woke the agent,
/// rather than a prompt the user typed.
pub(crate) const TASK_NOTIFICATION_KIND: &str = "task-notification";

/// Code Mode's tool: one call whose JavaScript runs other tools.
const SCRIPT_TOOL: &str = "REPL";
/// Code Mode's tool that waits for background scripts; it draws nothing.
const POLL_TOOL: &str = "Poll";
/// The model a virtual record, one Claude Code wrote for a script, names.
const SYNTHETIC_MODEL: &str = "<synthetic>";
/// The wrappers around a `REPL` result's text.
const SCRIPT_RESULT_TAGS: [&str; 4] = ["<repl-output>", "</repl-output>", "<error>", "</error>"];

type Known<'a> = &'a dyn Fn(&str) -> Option<ChatEntry>;

/// What one complete transcript line adds to the chat, in record order.
/// `known` answers the entry already held under an id: a tool result comes
/// back as its whole card with the result set, and a `!` command's output as
/// its row with the output appended. `book` holds the chat's scripts across
/// lines: a delivery that settles a background script answers with that
/// script's card. A line that does not parse, has no `uuid`, or is not a
/// message yields nothing.
pub(crate) fn chat_entries(line: &str, known: Known<'_>, book: &mut ScriptBook) -> Vec<ChatEntry> {
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some(uuid) = str_at(&record, "uuid").filter(|uuid| !uuid.is_empty()) else {
        return Vec::new();
    };
    let kind = str_at(&record, "type");
    let origin = origin_of(&record);
    book.begin_row(
        origin,
        kind == Some("user") && record_facts(&record).is_user_prompt,
    );
    let entries = match kind {
        Some("user") => user_entries(&record, uuid, known, book),
        Some("assistant") => assistant_entries(&record, uuid, known, book, origin),
        Some("attachment") => attachment_entries(&record, uuid, known, book),
        Some("system") => system_entry(&record, uuid).into_iter().collect(),
        _ => Vec::new(),
    };
    book.end_row();
    entries
}

/// Who wrote `record`: an assistant record is the model's unless it is
/// virtual, which Claude Code wrote for a script.
fn origin_of(record: &Value) -> Origin {
    if str_at(record, "type") != Some("assistant") {
        return Origin::Other;
    }
    let synthetic = flag(record, "isVirtual")
        || record.pointer("/message/model").and_then(Value::as_str) == Some(SYNTHETIC_MODEL);
    if synthetic {
        Origin::Synthetic
    } else {
        Origin::Model
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
    record_facts(&record)
}

/// [`line_facts`] for a record already parsed.
pub(crate) fn record_facts(record: &Value) -> TranscriptLineFacts {
    let prompt = prompt_text(record);
    let is_interrupt = prompt.as_deref().is_some_and(is_interrupt_text);
    TranscriptLineFacts {
        prompt_id: str_at(record, "promptId").map(str::to_owned),
        is_user_prompt: prompt.is_some() && !is_interrupt,
        is_interrupt,
        timestamp: str_at(record, "timestamp")
            .and_then(|stamp| DateTime::parse_from_rfc3339(stamp).ok())
            .map(|stamp| stamp.with_timezone(&Utc)),
        ended_tasks: ended_tasks(record),
        model: reply_model(record),
        effort: reply_effort(record),
        model_switch: model_switch(record),
    }
}

/// Whether `record` is the session's own reply: the model answered, not a
/// subagent and not a virtual record Claude Code wrote.
fn is_main_reply(record: &Value) -> bool {
    origin_of(record) == Origin::Model && !flag(record, "isSidechain")
}

/// The model a main-thread reply names: none for a subagent's record, a
/// virtual one Claude Code wrote, or any record that is not a reply.
fn reply_model(record: &Value) -> Option<String> {
    if !is_main_reply(record) {
        return None;
    }
    record
        .pointer("/message/model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
}

/// The effort a main-thread reply answered with, as the record's top-level
/// field names it: none for a subagent's record, a virtual one Claude Code
/// wrote, any record that is not a reply, or a reply that names no effort.
fn reply_effort(record: &Value) -> Option<String> {
    if !is_main_reply(record) {
        return None;
    }
    str_at(record, "effort")
        .filter(|effort| !effort.is_empty())
        .map(str::to_owned)
}

/// The model switch a main-thread command's output records: none for a
/// subagent's record, context Claude Code injected, a compact summary, a
/// prompt that merely quotes a command's output, or an output that names no
/// switch. Only the user's own record whose text starts with the output tag
/// is the command's output; a prompt the user typed can quote one, so the tag
/// anywhere in the text is not enough.
fn model_switch(record: &Value) -> Option<ModelSwitch> {
    if str_at(record, "type") != Some("user")
        || is_meta(record)
        || flag(record, "isCompactSummary")
        || flag(record, "isSidechain")
    {
        return None;
    }
    let text = record.pointer("/message/content").and_then(content_text)?;
    let output = text.trim_start();
    if !output.starts_with("<local-command-stdout>") {
        return None;
    }
    records::tag_values(output, "<local-command-stdout>", "</local-command-stdout>")
        .next()
        .and_then(parse_model_switch)
}

/// The switch a command's output records: `/model` prints "Set model to",
/// the new model's label in backticks, then "for this session only". A typed
/// `/model <name>` adds "and saved as your default for new sessions", and
/// either names the effort in backticks after "with" when the effort moved
/// too. "Kept model as" keeps the model, so it is no switch.
fn parse_model_switch(text: &str) -> Option<ModelSwitch> {
    let label = text.strip_prefix("Set model to `")?;
    let (label, rest) = label.split_once('`')?;
    if label.is_empty() {
        return None;
    }
    Some(ModelSwitch {
        label: label.to_owned(),
        effort: switched_effort(rest),
    })
}

/// The effort a switch names after its label: the backticked word between
/// "with" and "effort", and none when the switch names no effort.
fn switched_effort(rest: &str) -> Option<String> {
    let (_, after) = rest.split_once(" with `")?;
    let (effort, tail) = after.split_once('`')?;
    (!effort.is_empty() && tail.starts_with(" effort")).then(|| effort.to_owned())
}

/// The tasks a task-notification record reports ended: each notification's
/// task id. A monitor's event arrives as a notification too, carrying an
/// `<event>`, and leaves its monitor running, so that notification is not
/// listed; the others in the same record are.
fn ended_tasks(record: &Value) -> Vec<String> {
    if record.pointer("/origin/kind").and_then(Value::as_str) != Some(TASK_NOTIFICATION_KIND) {
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

fn user_entries(
    record: &Value,
    uuid: &str,
    known: Known<'_>,
    book: &mut ScriptBook,
) -> Vec<ChatEntry> {
    if is_meta(record) || flag(record, "isCompactSummary") {
        return Vec::new();
    }
    let Some(content) = record.pointer("/message/content") else {
        return Vec::new();
    };
    if record.pointer("/origin/kind").and_then(Value::as_str) == Some(TASK_NOTIFICATION_KIND) {
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
        Value::Array(blocks) => user_block_entries(record, uuid, blocks, known, book),
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
/// "Interrupted", a slash command its command line as typed (`/clear`
/// none), a local command's output a system row, other command noise and
/// blank text are nothing.
fn user_message(uuid: &str, text: &str) -> Option<ChatEntry> {
    if is_interrupt_text(text) {
        return Some(system_row(uuid, "Interrupted".to_owned()));
    }
    if let Some(line) = records::command_line(text) {
        return command_bubble(uuid, line);
    }
    if text.trim_start().starts_with("<local-command-stdout>") {
        return local_output_row(uuid, text);
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

/// A local slash command's output (`/model`'s "Set model to …", `/clear`'s
/// "cleared") as a system row: the `<local-command-stdout>` element's text
/// without its SGR escapes, trimmed; none when that is empty, the CLI's
/// "(no content)" placeholder, or `/compact`'s "Compacted …" output, which
/// the "Conversation compacted" row already marks.
fn local_output_row(uuid: &str, text: &str) -> Option<ChatEntry> {
    let output = strip_sgr(tag_text(text, "local-command-stdout")?);
    let output = output.trim();
    let silent = output.is_empty() || output == "(no content)" || output.starts_with("Compacted");
    (!silent).then(|| system_row(uuid, output.to_owned()))
}

/// A slash command's command line as the user's bubble; none for `/clear`,
/// which the chat marks by starting over.
fn command_bubble(uuid: &str, line: String) -> Option<ChatEntry> {
    (!records::is_clear(&line)).then(|| ChatEntry::User {
        id: uuid.to_owned(),
        text: line,
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
    book: &mut ScriptBook,
) -> Vec<ChatEntry> {
    let mut entries = Vec::new();
    let mut texts = Vec::new();
    for block in blocks {
        match str_at(block, "type") {
            Some("tool_result") => entries.extend(tool_result_entries(record, block, known, book)),
            Some("text") => texts.extend(str_at(block, "text")),
            _ => {}
        }
    }
    if !texts.is_empty() {
        entries.extend(user_message(uuid, &texts.join("\n")));
    }
    entries
}

/// A tool result's entries: a background script's receipt numbers its card
/// and leaves it running; a script's error call fails its script; any other
/// result settles the scripts its delivery names, then answers its own card.
fn tool_result_entries(
    record: &Value,
    block: &Value,
    known: Known<'_>,
    book: &mut ScriptBook,
) -> Vec<ChatEntry> {
    let Some(tool_use_id) = str_at(block, "tool_use_id") else {
        return Vec::new();
    };
    let text = block
        .get("content")
        .and_then(content_text)
        .unwrap_or_default();
    let mut entries = Vec::new();
    if book.is_script(tool_use_id) {
        if let Some(eval) = scripts::dispatched_eval(&text) {
            book.dispatched(tool_use_id, eval);
            return script_update(known, tool_use_id, |_, facts| facts.eval = Some(eval))
                .into_iter()
                .collect();
        }
        book.at_once(tool_use_id);
    } else if let Some(owner) = book.error_call_owner(tool_use_id) {
        return script_update(known, &owner, |result, facts| {
            fail(result);
            facts.error = first_error_line(&text).or(facts.error.take());
        })
        .into_iter()
        .collect();
    } else {
        entries = settled_entries(book, scripts::poll_events(&text), known);
    }
    entries.extend(answered_card(record, block, tool_use_id, &text, known));
    entries
}

/// The cards of the scripts `events` settle: each done or failed, with the
/// event's run time and, for a failure without one, its first error line.
fn settled_entries(
    book: &mut ScriptBook,
    events: Vec<ReplEvent>,
    known: Known<'_>,
) -> Vec<ChatEntry> {
    book.settle(events)
        .into_iter()
        .filter_map(|(id, event)| {
            script_update(known, &id, |result, facts| {
                *result = Some(ToolOutcome {
                    text: cap_output(&event.text, TOOL_RESULT_CAP),
                    is_error: !event.ok,
                });
                facts.ms = event.ms.or(facts.ms);
                if facts.error.is_none() {
                    facts.error = event.error.map(|line| cap_chars(&line, SUMMARY_MAX_CHARS));
                }
            })
        })
        .collect()
}

/// Marks a script's outcome failed, keeping any text it has.
fn fail(result: &mut Option<ToolOutcome>) {
    let text = result
        .take()
        .map(|outcome| outcome.text)
        .unwrap_or_default();
    *result = Some(ToolOutcome {
        text,
        is_error: true,
    });
}

/// Script `id`'s held card with `change` made to its outcome and facts;
/// none when the chat holds no script card under `id`.
fn script_update(
    known: Known<'_>,
    id: &str,
    change: impl FnOnce(&mut Option<ToolOutcome>, &mut ScriptFacts),
) -> Option<ChatEntry> {
    let mut entry = known(id)?;
    let ChatEntry::Tool {
        result,
        script: Some(facts),
        ..
    } = &mut entry
    else {
        return None;
    };
    change(result, facts);
    Some(entry)
}

/// A held card answered by its tool result `block`, whose text is `text`,
/// and the receipts the result carries.
fn answered_card(
    record: &Value,
    block: &Value,
    tool_use_id: &str,
    text: &str,
    known: Known<'_>,
) -> Vec<ChatEntry> {
    let Some(ChatEntry::Tool {
        id,
        name,
        summary,
        input,
        parent_id,
        tail_paths,
        kind,
        script,
        ..
    }) = known(tool_use_id)
    else {
        return Vec::new();
    };
    let is_error = flag(block, "is_error");
    let script = script.map(|facts| ScriptFacts {
        error: if is_error {
            first_error_line(text)
        } else {
            None
        },
        ..facts
    });
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
            text: cap_output(text, TOOL_RESULT_CAP),
            is_error,
        }),
        parent_id,
        tail_paths,
        kind,
        script,
    };
    std::iter::once(card).chain(receipts).collect()
}

/// A failed script's first error line: the first line of its result with
/// text once the `<repl-output>` and `<error>` wrappers are taken off.
fn first_error_line(text: &str) -> Option<String> {
    text.lines()
        .map(|line| {
            SCRIPT_RESULT_TAGS
                .iter()
                .fold(line.to_owned(), |line, tag| line.replace(tag, ""))
        })
        .map(|line| line.trim().to_owned())
        .find(|line| !line.is_empty())
        .map(|line| cap_chars(&line, SUMMARY_MAX_CHARS))
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
/// `origin` says whether the model or a script made the record's calls.
fn assistant_entries(
    record: &Value,
    uuid: &str,
    known: Known<'_>,
    book: &mut ScriptBook,
    origin: Origin,
) -> Vec<ChatEntry> {
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
            Some("tool_use") => {
                entries.extend(call_entries(
                    block,
                    str_at(record, "cwd"),
                    known,
                    book,
                    origin,
                ));
            }
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

/// What a tool call adds: none for a `Poll`; for a script's virtual call,
/// its card naming the script as its parent, or, for a script's error call,
/// no card but the script's card marked failed (nothing when no single
/// script owns the call); for the model's `REPL` call, a script's card.
fn call_entries(
    block: &Value,
    cwd: Option<&str>,
    known: Known<'_>,
    book: &mut ScriptBook,
    origin: Origin,
) -> Vec<ChatEntry> {
    let (Some(id), Some(name)) = (str_at(block, "id"), str_at(block, "name")) else {
        return Vec::new();
    };
    if name == POLL_TOOL {
        return Vec::new();
    }
    let mut parent = None;
    match origin {
        Origin::Synthetic => match book.tie(id) {
            Some(Tie::Error(script)) => {
                return script_update(known, &script, |result, _| fail(result))
                    .into_iter()
                    .collect();
            }
            Some(Tie::Inner(script)) => parent = Some(script),
            None if id.starts_with(scripts::ERROR_PREFIX) => return Vec::new(),
            None => {}
        },
        Origin::Model if name == SCRIPT_TOOL => book.open(id),
        Origin::Model | Origin::Other => {}
    }
    vec![tool_card(block, id, name, cwd, parent)]
}

/// A tool call's card. A shell call names the files its command writes or
/// logs to, resolved against `cwd`, the record's working folder. A `REPL`
/// call carries its script facts, its description's first line read before
/// the input is capped; a call a script ran takes `parent`, that script.
fn tool_card(
    block: &Value,
    id: &str,
    name: &str,
    cwd: Option<&str>,
    parent: Option<String>,
) -> ChatEntry {
    let input = block.get("input").unwrap_or(&Value::Null);
    let tail_paths = match (Shell::of_tool(name), str_at(input, "command")) {
        (Some(shell), Some(command)) => tail_paths(shell, command, cwd),
        _ => Vec::new(),
    };
    let kind = tool_kind(name);
    let facts = (kind == ToolKind::Script).then(|| ScriptFacts {
        description: scripts::description_line(input),
        ..ScriptFacts::default()
    });
    ChatEntry::Tool {
        id: id.to_owned(),
        name: name.to_owned(),
        summary: tool_summary(name, input),
        input: cap(&format!("{input:#}"), TOOL_INPUT_CAP),
        result: None,
        parent_id: parent,
        tail_paths,
        kind,
        script: facts,
    }
}

/// What sort of work Claude's tool `name` does: reading, searching, running
/// a command, running a script of other calls (Code Mode's `REPL`), or
/// anything else (an edit, a write, a subagent task, an MCP tool).
pub(crate) fn tool_kind(name: &str) -> ToolKind {
    match name {
        SCRIPT_TOOL => ToolKind::Script,
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
        "Agent" | "Task" | SCRIPT_TOOL => "description",
        _ => "",
    };
    let line = str_at(input, key)
        .and_then(|value| value.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or("");
    cap_chars(line, SUMMARY_MAX_CHARS)
}

/// An attachment record's entries: a delivery of background scripts' events
/// (`poll_events` with envelopes) settles those scripts' cards; a queued
/// message is its own entry.
fn attachment_entries(
    record: &Value,
    uuid: &str,
    known: Known<'_>,
    book: &mut ScriptBook,
) -> Vec<ChatEntry> {
    let envelopes = record
        .pointer("/attachment/envelopes")
        .and_then(Value::as_array)
        .filter(|_| {
            record.pointer("/attachment/type").and_then(Value::as_str) == Some("poll_events")
        });
    let events: Vec<ReplEvent> = envelopes
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .flat_map(scripts::queued_events)
        .collect();
    let mut entries = settled_entries(book, events, known);
    entries.extend(queued_entry(record, uuid));
    entries
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
            if let Some(line) = records::command_line(content) {
                return command_bubble(uuid, line);
            }
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
pub(crate) fn cap_chars(text: &str, max: usize) -> String {
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
        CUT_MARKER, Known, TOOL_INPUT_CAP, TOOL_RESULT_CAP, cap, cap_output, line_facts,
        split_at_limit, tool_kind,
    };
    use crate::scripts::ScriptBook;
    use agent_api::{ModelSwitch, TranscriptLineFacts};
    use chrono::{TimeDelta, TimeZone as _, Utc};
    use protocol::{ChatEntry, ScriptFacts, ToolKind, ToolOutcome};
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
                model: None,
                effort: None,
                model_switch: None,
            }
        );
    }

    /// A main-thread `user` record carrying `output` as its command's standard
    /// output.
    fn command_output(output: &str) -> String {
        json!({
            "type": "user", "uuid": uid("52"),
            "message": {"role": "user",
                "content": format!("<local-command-stdout>{output}</local-command-stdout>")},
        })
        .to_string()
    }

    #[test]
    fn a_session_only_switch_reads_as_a_model_switch() {
        let line = command_output("Set model to `Sonnet 5.5` for this session only");
        assert_eq!(
            line_facts(&line).model_switch,
            Some(ModelSwitch {
                label: "Sonnet 5.5".to_owned(),
                effort: None,
            })
        );
    }

    #[test]
    fn a_switch_with_effort_names_its_effort() {
        let line =
            command_output("Set model to `Opus 5.5` for this session only with `xhigh` effort");
        assert_eq!(
            line_facts(&line).model_switch,
            Some(ModelSwitch {
                label: "Opus 5.5".to_owned(),
                effort: Some("xhigh".to_owned()),
            })
        );
    }

    #[test]
    fn a_saved_switch_reads_as_a_model_switch() {
        let line =
            command_output("Set model to `Opus 5.5` and saved as your default for new sessions");
        assert_eq!(
            line_facts(&line).model_switch,
            Some(ModelSwitch {
                label: "Opus 5.5".to_owned(),
                effort: None,
            })
        );
    }

    #[test]
    fn a_kept_model_is_no_switch() {
        let line = json!({
            "type": "system", "subtype": "local_command", "uuid": uid("53"),
            "content": "<local-command-stdout>Kept model as `Sonnet 5.5`</local-command-stdout>",
        })
        .to_string();
        assert_eq!(line_facts(&line).model_switch, None);
    }

    #[test]
    fn a_sidechain_switch_is_none() {
        let line = json!({
            "type": "user", "uuid": uid("54"), "isSidechain": true,
            "message": {"role": "user",
                "content": "<local-command-stdout>Set model to `Sonnet 5.5` for this session only</local-command-stdout>"},
        })
        .to_string();
        assert_eq!(line_facts(&line).model_switch, None);
    }

    #[test]
    fn a_prompt_quoting_a_switch_is_no_switch() {
        let line = json!({
            "type": "user", "uuid": uid("55"),
            "message": {"role": "user",
                "content": "see <local-command-stdout>Set model to `Sonnet 5.5` for this session only</local-command-stdout>"},
        })
        .to_string();
        assert_eq!(line_facts(&line).model_switch, None);
    }

    #[test]
    fn a_meta_record_is_no_switch() {
        let line = json!({
            "type": "user", "uuid": uid("56"), "isMeta": true,
            "message": {"role": "user",
                "content": "<local-command-stdout>Set model to `Sonnet 5.5` for this session only</local-command-stdout>"},
        })
        .to_string();
        assert_eq!(line_facts(&line).model_switch, None);
    }

    #[test]
    fn a_compact_summary_is_no_switch() {
        let line = json!({
            "type": "user", "uuid": uid("57"), "isCompactSummary": true,
            "message": {"role": "user",
                "content": "<local-command-stdout>Set model to `Sonnet 5.5` for this session only</local-command-stdout>"},
        })
        .to_string();
        assert_eq!(line_facts(&line).model_switch, None);
    }

    /// An assistant reply naming `model`, with `extra` fields merged in.
    fn reply_naming(model: &str, extra: &Value) -> String {
        let mut record = json!({
            "type": "assistant", "uuid": uid("50"),
            "message": {"role": "assistant", "model": model, "content": [{"type": "text", "text": "Done."}]},
        });
        if let (Some(record), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
            record.extend(extra.clone());
        }
        record.to_string()
    }

    #[test]
    fn a_main_reply_names_its_model_in_the_line_facts() {
        let line = reply_naming("claude-opus-5-5", &json!({}));
        assert_eq!(line_facts(&line).model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn a_main_reply_names_its_effort_in_the_line_facts() {
        let line = reply_naming("claude-opus-5-5", &json!({"effort": "high"}));
        assert_eq!(line_facts(&line).effort.as_deref(), Some("high"));
    }

    #[test]
    fn a_sidechain_reply_names_no_effort() {
        let line = reply_naming(
            "claude-opus-5-5",
            &json!({"isSidechain": true, "effort": "high"}),
        );
        assert_eq!(line_facts(&line).effort, None);
    }

    #[test]
    fn a_main_reply_without_effort_names_none() {
        let line = reply_naming("claude-opus-5-5", &json!({}));
        assert_eq!(line_facts(&line).effort, None);
    }

    #[test]
    fn a_sidechain_reply_names_no_model() {
        let line = reply_naming("claude-opus-5-5", &json!({"isSidechain": true}));
        assert_eq!(line_facts(&line).model, None);
    }

    #[test]
    fn a_synthetic_reply_names_no_model() {
        let line = reply_naming("<synthetic>", &json!({}));
        assert_eq!(line_facts(&line).model, None);
    }

    #[test]
    fn a_user_record_names_no_model() {
        let line = json!({
            "type": "user", "uuid": uid("51"),
            "message": {"role": "user", "model": "claude-opus-5-5", "content": "Hello there"},
        })
        .to_string();
        assert_eq!(line_facts(&line).model, None);
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

    /// One line read alone, with a fresh book.
    fn chat_entries(line: &str, known: Known<'_>) -> Vec<ChatEntry> {
        super::chat_entries(line, known, &mut ScriptBook::default())
    }

    /// Every line of `body` through [`super::chat_entries`] with one book,
    /// each output upserted by id into the chat the next line's `known`
    /// reads: the per-line outputs and the chat at the end.
    fn replay(body: &str) -> (Vec<Vec<ChatEntry>>, Vec<ChatEntry>) {
        let mut chat: Vec<ChatEntry> = Vec::new();
        let mut outputs = Vec::new();
        let mut book = ScriptBook::default();
        for line in body.lines() {
            let out = {
                let held = &chat;
                let known = |id: &str| held.iter().find(|e| id_of(e) == id).cloned();
                super::chat_entries(line, &known, &mut book)
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
            script: None,
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
            "stream-json-run" => {
                include_str!("../tests/fixtures/transcripts/stream-json-run.jsonl")
            }
            "subagent-run" => include_str!("../tests/fixtures/transcripts/subagent-run.jsonl"),
            "repl" => include_str!("../tests/fixtures/transcripts/repl.jsonl"),
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

    const SCRIPT: &str = "toolu_r1";
    const FAILING_SCRIPT: &str = "toolu_r2";
    const INNER: [&str; 2] = [
        "repl_00000000-0000-4000-8000-0000000000a1",
        "repl_00000000-0000-4000-8000-0000000000a2",
    ];

    fn tool_in<'a>(chat: &'a [ChatEntry], wanted: &str) -> &'a ChatEntry {
        chat.iter()
            .find(|entry| id_of(entry) == wanted)
            .unwrap_or_else(|| unreachable!("no entry {wanted} in {chat:?}"))
    }

    fn parent_of<'a>(chat: &'a [ChatEntry], wanted: &str) -> Option<&'a str> {
        match tool_in(chat, wanted) {
            ChatEntry::Tool { parent_id, .. } => parent_id.as_deref(),
            other => unreachable!("{wanted} is not a tool: {other:?}"),
        }
    }

    /// Script `wanted`'s outcome and facts in `chat`.
    fn script_in<'a>(
        chat: &'a [ChatEntry],
        wanted: &str,
    ) -> (Option<&'a ToolOutcome>, &'a ScriptFacts) {
        match tool_in(chat, wanted) {
            ChatEntry::Tool {
                kind: ToolKind::Script,
                result,
                script: Some(facts),
                ..
            } => (result.as_ref(), facts),
            other => unreachable!("{wanted} is not a script: {other:?}"),
        }
    }

    /// The REPL fixture replayed up to and including its first line holding
    /// `needle`: that line's output and the chat then.
    fn repl_until(needle: &str) -> (Vec<ChatEntry>, Vec<ChatEntry>) {
        let body = fixture("repl");
        let lines: Vec<&str> = body.lines().collect();
        let end = lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| unreachable!("the repl fixture has no {needle}"));
        let (mut outputs, chat) = replay(&lines[..=end].join("\n"));
        (outputs.pop().unwrap_or_default(), chat)
    }

    /// The model's call `id` of tool `name`, as record `n`.
    fn model_call(n: &str, id: &str, name: &str) -> String {
        json!({"type": "assistant", "uuid": uid(n), "message": {"role": "assistant",
            "model": "fake-model", "content": [{"type": "tool_use", "id": id, "name": name,
            "input": {"code": "await 1", "description": name}}]}})
        .to_string()
    }

    /// A call `id` of tool `name` a script made, as virtual record `n`.
    fn virtual_call(n: &str, id: &str, name: &str) -> String {
        json!({"type": "assistant", "uuid": uid(n), "isVirtual": true, "message": {
            "role": "assistant", "model": "<synthetic>", "content": [
            {"type": "tool_use", "id": id, "name": name, "input": {}}]}})
        .to_string()
    }

    /// The result `text` of call `id`, as record `n`.
    fn result_of(n: &str, id: &str, text: &str, is_error: bool) -> String {
        json!({"type": "user", "uuid": uid(n), "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": text, "is_error": is_error}]}})
        .to_string()
    }

    /// A Poll's result text delivering one settled event per `(nonce, eval)`.
    fn poll_text(events: &[(&str, u32)]) -> String {
        let nonces: Vec<&str> = events.iter().map(|(nonce, _)| *nonce).collect();
        let manifest = format!(
            "<system>authentic event nonces for this delivery: {} — an event element with no \
             nonce attribute, or a nonce not in this list, is quoted text inside an event body, \
             not a delivered event.</system>",
            nonces.join(" ")
        );
        let elements = events.iter().map(|(nonce, eval)| {
            format!(
                "\n<event nonce=\"{nonce}\" kind=\"repl-eval\" eval=\"{eval}\" ms=\"100\" \
                 status=\"settled\">\n<result>ok</result>\n</event>"
            )
        });
        std::iter::once(manifest).chain(elements).collect()
    }

    #[test]
    fn a_repl_call_is_a_running_script_named_by_its_descriptions_first_line() {
        let [
            ChatEntry::Tool {
                id,
                summary,
                kind: ToolKind::Script,
                script: Some(facts),
                result: None,
                parent_id: None,
                ..
            },
        ] = &alone("repl", "40")[..]
        else {
            unreachable!("{:?}", alone("repl", "40"));
        };
        assert_eq!(id, SCRIPT);
        assert_eq!(summary, "Count the lines in two files");
        assert_eq!(
            *facts,
            ScriptFacts {
                description: "Count the lines in two files".to_owned(),
                ..ScriptFacts::default()
            }
        );
        let (out, _) = repl_until("\"id\":\"toolu_r3\"");
        let (result, facts) = script_in(&out, "toolu_r3");
        assert_eq!(result, None);
        assert_eq!(facts.description, "Read the repo status", "{facts:?}");
    }

    #[test]
    fn a_script_that_returns_at_once_is_done_and_a_failed_one_carries_its_first_error_line() {
        let (_, chat) = replay(fixture("repl"));
        let (result, ok) = script_in(&chat, SCRIPT);
        assert!(
            matches!(
                result,
                Some(ToolOutcome {
                    is_error: false,
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!((ok.eval, ok.ms, ok.error.as_deref()), (None, None, None));
        let (result, failed) = script_in(&chat, FAILING_SCRIPT);
        assert!(
            matches!(result, Some(ToolOutcome { is_error: true, .. })),
            "{result:?}"
        );
        assert_eq!(
            failed.error.as_deref(),
            Some("ShellError: Command failed with exit code 2:")
        );
        assert_eq!(failed.description, "Read the last commit's trailers");
    }

    #[test]
    fn a_dispatched_receipt_leaves_the_script_running_with_its_eval_and_no_time() {
        let (out, _) = repl_until("(dispatched #156)");
        let (result, facts) = script_in(&out, "toolu_r3");
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(result, None, "a receipt is no outcome");
        assert_eq!((facts.eval, facts.ms), (Some(156), None));
    }

    #[test]
    fn a_poll_settling_an_eval_finishes_its_script_with_the_events_time() {
        let (out, chat) = repl_until("\"tool_use_id\":\"toolu_p1\"");
        assert_eq!(out.len(), 1, "only the settled script: {out:?}");
        let (result, facts) = script_in(&out, "toolu_r3");
        assert_eq!(
            result,
            Some(&ToolOutcome {
                text: "status:\n## main\n\nplan:\nfive lines".to_owned(),
                is_error: false,
            })
        );
        assert_eq!(
            (facts.eval, facts.ms, facts.error.as_deref()),
            (Some(156), Some(4194), None)
        );
        let (other, facts) = script_in(&chat, "toolu_r4");
        assert_eq!(
            (other, facts.eval),
            (None, Some(157)),
            "eval 157 still runs"
        );

        let (out, _) = repl_until("\"tool_use_id\":\"toolu_p2\"");
        let (result, facts) = script_in(&out, "toolu_r4");
        assert!(
            matches!(
                result,
                Some(ToolOutcome {
                    is_error: false,
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(facts.ms, Some(842));
    }

    #[test]
    fn a_poll_whose_event_opens_with_console_output_still_settles_its_script() {
        let (out, _) = repl_until("\"tool_use_id\":\"toolu_p4\"");
        let (result, facts) = script_in(&out, "toolu_r7");
        assert_eq!(
            result,
            Some(&ToolOutcome {
                text: "{\"taskId\":\"t1\",\"timeoutMs\":1800000,\"persistent\":false}".to_owned(),
                is_error: false,
            })
        );
        assert_eq!((facts.eval, facts.ms), (Some(200), Some(3715)));
    }

    #[test]
    fn a_long_description_is_cut_to_the_summarys_length() {
        let call = model_call("h1", "toolu_long", "REPL").replace(
            "\"description\":\"REPL\"",
            &format!("\"description\":\"{}\"", "w".repeat(300)),
        );
        let [
            ChatEntry::Tool {
                summary,
                script: Some(facts),
                ..
            },
        ] = &chat_entries(&call, &|_| None)[..]
        else {
            unreachable!("{call}");
        };
        assert_eq!(facts.description.chars().count(), 120, "{facts:?}");
        assert_eq!(&facts.description, summary);
    }

    #[test]
    fn a_poll_settling_an_eval_with_an_error_fails_its_script_with_its_line_and_time() {
        let (out, _) = repl_until("\"tool_use_id\":\"toolu_p3\"");
        let (result, facts) = script_in(&out, "toolu_r5");
        assert!(
            matches!(result, Some(ToolOutcome { is_error: true, .. })),
            "{result:?}"
        );
        assert_eq!(facts.ms, Some(34020));
        let error = facts.error.as_deref().unwrap_or_default();
        assert!(
            error.starts_with("Error: REPL execution timed out after 30000ms of script time"),
            "{error}"
        );
        assert_eq!(
            error.chars().count(),
            120,
            "capped as a summary is: {error}"
        );
    }

    #[test]
    fn a_repl_error_call_draws_nothing_and_fails_the_one_script_in_flight() {
        let (out, _) = repl_until("\"id\":\"repl-error-");
        assert!(
            matches!(&out[..], [ChatEntry::Tool { id, .. }] if id == "toolu_r5"),
            "{out:?}"
        );
        let (outputs, chat) = replay(fixture("repl"));
        assert!(
            !chat
                .iter()
                .any(|entry| id_of(entry).starts_with("repl-error-")),
            "{chat:?}"
        );
        assert_eq!(
            outputs
                .iter()
                .flatten()
                .filter(|e| id_of(e).starts_with("repl-error-"))
                .count(),
            0
        );
        let (result, facts) = script_in(&chat, "toolu_r5");
        assert!(
            matches!(result, Some(ToolOutcome { is_error: true, .. })),
            "{result:?}"
        );
        assert_eq!(
            facts.error.as_deref(),
            Some("Timed out after 30s, not counting time spent in tool calls"),
            "the error call's line wins over the event's"
        );
        assert_eq!(facts.ms, Some(34020));
    }

    #[test]
    fn a_repl_error_call_no_single_script_owns_draws_nothing() {
        let error_call = virtual_call("e1", "repl-error-x", "REPL");
        let error_result = result_of("e2", "repl-error-x", "Error: boom", true);
        let (outputs, chat) = replay(&[error_call.clone(), error_result.clone()].join("\n"));
        assert_eq!(
            outputs,
            [Vec::<ChatEntry>::new(), Vec::new()],
            "none in flight"
        );
        assert_eq!(chat, []);

        let two = [
            model_call("e3", "toolu_q1", "REPL"),
            model_call("e4", "toolu_q2", "REPL"),
            result_of(
                "e5",
                "toolu_q1",
                "<repl-output>\n<result>a</result>\n</repl-output>",
                false,
            ),
            result_of(
                "e6",
                "toolu_q2",
                "<repl-output>\n<result>b</result>\n</repl-output>",
                false,
            ),
            error_call,
            error_result,
        ];
        let (outputs, chat) = replay(&two.join("\n"));
        assert_eq!(
            outputs[4..],
            [Vec::<ChatEntry>::new(), Vec::new()],
            "two in flight"
        );
        for id in ["toolu_q1", "toolu_q2"] {
            let (result, facts) = script_in(&chat, id);
            assert!(
                matches!(
                    result,
                    Some(ToolOutcome {
                        is_error: false,
                        ..
                    })
                ),
                "{id}"
            );
            assert_eq!(facts.error, None, "{id}");
        }
    }

    #[test]
    fn calls_after_a_single_at_once_result_are_that_scripts() {
        let (_, chat) = replay(fixture("repl"));
        for inner in INNER {
            assert_eq!(parent_of(&chat, inner), Some(SCRIPT), "{inner}");
        }
        assert_eq!(
            parent_of(&chat, "repl_00000000-0000-4000-8000-0000000000b1"),
            Some(FAILING_SCRIPT)
        );
        assert_eq!(
            parent_of(&chat, SCRIPT),
            None,
            "a model call is no script's"
        );
        assert!(
            matches!(
                tool_in(&chat, INNER[0]),
                ChatEntry::Tool {
                    kind: ToolKind::Run,
                    result: Some(_),
                    ..
                }
            ),
            "{chat:?}"
        );
    }

    #[test]
    fn calls_after_a_delivery_of_one_script_are_that_scripts() {
        let (_, chat) = replay(fixture("repl"));
        for (inner, script) in [
            ("c1", "toolu_r3"),
            ("c2", "toolu_r3"),
            ("c3", "toolu_r4"),
            ("d1", "toolu_r5"),
            ("e1", "toolu_r6"),
        ] {
            let id = format!("repl_{}", uid(inner));
            assert_eq!(parent_of(&chat, &id), Some(script), "{id}");
        }
        let (result, facts) = script_in(&chat, "toolu_r6");
        assert!(
            matches!(
                result,
                Some(ToolOutcome {
                    is_error: false,
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(
            (facts.eval, facts.ms),
            (Some(159), Some(2149)),
            "a poll_events delivery"
        );
    }

    #[test]
    fn calls_after_a_delivery_of_two_scripts_or_two_at_once_results_stay_untied() {
        let delivered = [
            model_call("f1", "toolu_d1", "REPL"),
            result_of("f2", "toolu_d1", "(dispatched #4)", false),
            model_call("f3", "toolu_d2", "REPL"),
            result_of("f4", "toolu_d2", "(dispatched #5)", false),
            model_call("f5", "toolu_p", "Poll"),
            result_of(
                "f6",
                "toolu_p",
                &poll_text(&[("uxbJyUIU", 4), ("xPSh_Y56", 5)]),
                false,
            ),
            virtual_call("f7", "repl_r4", "Read"),
        ];
        let (_, chat) = replay(&delivered.join("\n"));
        assert_eq!(parent_of(&chat, "repl_r4"), None);
        for id in ["toolu_d1", "toolu_d2"] {
            let (result, facts) = script_in(&chat, id);
            assert!(
                matches!(
                    result,
                    Some(ToolOutcome {
                        is_error: false,
                        ..
                    })
                ),
                "{id}"
            );
            assert_eq!(facts.ms, Some(100), "{id}");
        }

        let at_once = [
            model_call("f8", "toolu_q1", "REPL"),
            model_call("f9", "toolu_q2", "REPL"),
            result_of("fa", "toolu_q1", "a", false),
            result_of("fb", "toolu_q2", "b", false),
            virtual_call("fc", "repl_q", "Bash"),
        ];
        let (_, chat) = replay(&at_once.join("\n"));
        assert_eq!(parent_of(&chat, "repl_q"), None);
    }

    #[test]
    fn a_call_after_a_model_record_or_a_prompt_stays_untied() {
        let after_model = [
            model_call("g1", "toolu_a", "REPL"),
            result_of("g2", "toolu_a", "(dispatched #1)", false),
            result_of("g3", "toolu_p", &poll_text(&[("AAAA", 1)]), false),
            model_call("g4", "toolu_z", "Bash"),
            virtual_call("g5", "repl_r9", "Read"),
        ];
        let (_, chat) = replay(&after_model.join("\n"));
        assert_eq!(parent_of(&chat, "repl_r9"), None);

        let prompt = json!({"type": "user", "uuid": uid("g8"), "promptId": "p-2",
            "message": {"role": "user", "content": "and now this"}});
        let after_prompt = [
            model_call("g6", "toolu_q", "REPL"),
            result_of("g7", "toolu_q", "done", false),
            prompt.to_string(),
            virtual_call("g9", "repl_q9", "Read"),
        ];
        let (_, chat) = replay(&after_prompt.join("\n"));
        assert_eq!(parent_of(&chat, "repl_q9"), None);
    }

    #[test]
    fn a_poll_call_and_its_result_draw_nothing() {
        let (outputs, chat) = replay(fixture("repl"));
        for poll in ["toolu_p1", "toolu_p2", "toolu_p3"] {
            assert!(
                !chat.iter().any(|entry| id_of(entry) == poll),
                "{poll} in {chat:?}"
            );
        }
        let lines: Vec<&str> = fixture("repl").lines().collect();
        for (line, out) in lines.iter().zip(&outputs) {
            if line.contains("\"name\":\"Poll\"") || line.contains("\"deliveredVia\":\"poll_tool\"")
            {
                assert_eq!(out, &Vec::<ChatEntry>::new(), "{line}");
            }
        }
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

    #[test]
    fn stream_json_records_parse_into_chat_entries() {
        const CALL: &str = "call_00_K092JZdBfhv39k56iu6h5240";
        let (outputs, _) = replay(fixture("stream-json-run"));

        for (index, kind) in [
            (0, "hook"),
            (1, "hook"),
            (2, "init"),
            (3, "thinking tokens"),
            (8, "result"),
        ] {
            assert_eq!(outputs[index], [], "the {kind} record yields nothing");
        }
        assert!(
            matches!(&outputs[5][..], [ChatEntry::Tool { id, name, result: None, .. }] if id == CALL && name == "Bash"),
            "{:?}",
            outputs[5]
        );
        assert!(
            matches!(
                &outputs[6][..],
                [ChatEntry::Tool { id, result: Some(ToolOutcome { is_error: false, .. }), .. }] if id == CALL
            ),
            "{:?}",
            outputs[6]
        );
        assert_eq!(
            outputs[7],
            [ChatEntry::Answer {
                id: "5f0c8a43-2d6e-4b7a-9c1e-3a8d7e6f1b20".to_owned(),
                text: "All gates passed.".to_owned(),
                live: false,
            }]
        );
    }

    #[test]
    fn subagent_transcript_fixture_parses_into_chat_entries() {
        const CALL: &str = "toolu_00a81c9e93fc4c039810";
        let (outputs, _) = replay(fixture("subagent-run"));

        assert_eq!(
            outputs[0],
            [ChatEntry::User {
                id: "ba94b131-98f4-4528-bf46-a24155bee58e".to_owned(),
                text: "List the files in src.".to_owned(),
                pending: false,
            }]
        );
        assert!(
            matches!(&outputs[1][..], [ChatEntry::Tool { id, name, result: None, .. }]
                if id == CALL && name == "Bash"),
            "{:?}",
            outputs[1]
        );
        assert!(
            matches!(
                &outputs[2][..],
                [ChatEntry::Tool { id, result: Some(ToolOutcome { text, is_error: false }), .. }]
                    if id == CALL && text == "main.rs\nlib.rs"
            ),
            "{:?}",
            outputs[2]
        );
        assert_eq!(
            outputs[3],
            [ChatEntry::Answer {
                id: "a57ebdbd-14b2-418b-98fa-6bec4c5d89f0".to_owned(),
                text: "There are two files.".to_owned(),
                live: false,
            }]
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
            script: None,
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
        assert_eq!(
            alone("compact", "73"),
            [user("73", "/model")],
            "the command line itself"
        );
        assert_eq!(alone("compact", "74"), [], "empty output");
    }

    #[test]
    fn a_slash_command_is_a_user_bubble_with_its_args() {
        let typed = "<command-name>/nextup</command-name>\n<command-message>nextup</command-message>\n<command-args>RT-483</command-args>";
        let as_string =
            json!({"type": "user", "uuid": "u", "message": {"content": typed}}).to_string();
        let as_blocks =
            json!({"type": "user", "uuid": "u", "message": {"content": [{"type": "text", "text": typed}]}})
                .to_string();
        let bubble = ChatEntry::User {
            id: "u".to_owned(),
            text: "/nextup RT-483".to_owned(),
            pending: false,
        };
        assert_eq!(
            chat_entries(&as_string, &|_| None),
            std::slice::from_ref(&bubble)
        );
        assert_eq!(chat_entries(&as_blocks, &|_| None), [bubble]);
    }

    #[test]
    fn a_slash_command_without_args_is_its_name_alone() {
        let line = json!({"type": "user", "uuid": "u", "message": {"content":
            "<command-name>/review</command-name>\n<command-message>review</command-message>\n<command-args></command-args>"}})
        .to_string();
        assert_eq!(
            chat_entries(&line, &|_| None),
            [ChatEntry::User {
                id: "u".to_owned(),
                text: "/review".to_owned(),
                pending: false
            }]
        );
    }

    #[test]
    fn clear_and_an_expanded_skill_draw_no_bubble() {
        let clear = json!({"type": "user", "uuid": "u", "message": {"content":
            "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>"}})
        .to_string();
        let expansion = json!({"type": "user", "uuid": "v", "isMeta": true, "message": {"content":
            "<command-name>/nextup</command-name>\nBase directory for this skill: x"}})
        .to_string();
        assert_eq!(chat_entries(&clear, &|_| None), []);
        assert_eq!(chat_entries(&expansion, &|_| None), []);
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
            if line.contains("<local-command-stdout>cleared") {
                assert_eq!(
                    chat_entries(line, &|_| None),
                    [system("9a", "cleared")],
                    "a local command's output is a row"
                );
                continue;
            }
            assert_eq!(chat_entries(line, &|_| None), [], "{line}");
        }
    }

    #[test]
    fn a_local_command_stdout_becomes_a_system_row() {
        let said = "Set model to Haiku 5.5 for this session only with high effort";
        let record = json!({"type": "user", "uuid": "u", "message": {"role": "user",
            "content": format!("<local-command-stdout>\u{1b}[1m{said}\u{1b}[22m\n</local-command-stdout>")}})
        .to_string();
        assert_eq!(
            chat_entries(&record, &|_| None),
            [ChatEntry::System {
                id: "u".to_owned(),
                text: said.to_owned()
            }]
        );
        let ticked = json!({"type": "user", "uuid": "v", "message": {"role": "user",
            "content": "<local-command-stdout>Set model to `Sonnet 5.5`</local-command-stdout>"}})
        .to_string();
        assert_eq!(
            chat_entries(&ticked, &|_| None),
            [ChatEntry::System {
                id: "v".to_owned(),
                text: "Set model to `Sonnet 5.5`".to_owned()
            }],
            "backticks kept"
        );
    }

    #[test]
    fn placeholder_and_compact_stdout_are_dropped() {
        let row = |content: &str| {
            let record = json!({"type": "user", "uuid": "u", "message": {"role": "user", "content": content}})
                .to_string();
            chat_entries(&record, &|_| None)
        };
        for content in [
            "<local-command-stdout>(no content)</local-command-stdout>",
            "<local-command-stdout>\u{1b}[2mCompacted (ctrl+o to see full summary)\u{1b}[22m\n\
             PreCompact [node hooks/fake-precompact.mjs] completed successfully</local-command-stdout>",
            "<local-command-stdout>Compacted</local-command-stdout>",
        ] {
            assert_eq!(row(content), [], "{content}");
        }
        for kept in ["Kept model as Sonnet 5.5", "cleared"] {
            let content = format!("<local-command-stdout>{kept}</local-command-stdout>");
            assert_eq!(
                row(&content),
                [ChatEntry::System {
                    id: "u".to_owned(),
                    text: kept.to_owned()
                }],
                "{kept}"
            );
        }
    }

    #[test]
    fn an_empty_local_command_stdout_is_dropped() {
        for content in [
            "<local-command-stdout></local-command-stdout>",
            "<local-command-stdout> \n </local-command-stdout>",
        ] {
            let record = json!({"type": "user", "uuid": "u", "message": {"role": "user", "content": content}})
                .to_string();
            assert_eq!(chat_entries(&record, &|_| None), [], "{content}");
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
