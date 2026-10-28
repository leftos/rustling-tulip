//! Decoding rules for Claude Code transcript records that every reader of a
//! transcript shares, so they agree on what a record means: a tool result's
//! text, a denied or interrupted call, the tasks a task notification names
//! and the background task a tool result launches.

use serde_json::Value;

/// The start of the result Claude Code writes for a tool call the user
/// turned down at a permission prompt.
pub(crate) const DENIED_RESULT_PREFIX: &str = "The user doesn't want to proceed with this tool use";

/// The start of the text Claude Code writes when the user interrupts a tool
/// call or a turn.
pub(crate) const INTERRUPTED_PREFIX: &str = "[Request interrupted by user";

/// The tag a task notification opens with; a line without it names no
/// finished task.
pub(crate) const NOTIFICATION_TAG: &str = "<task-notification>";

/// The text a `tool_result` block's `content` holds: the string itself, or
/// the first `text` part of an array.
pub(crate) fn result_text(block: &Value) -> Option<&str> {
    let content = &block["content"];
    content.as_str().or_else(|| {
        content
            .as_array()?
            .iter()
            .find(|part| part["type"] == "text")
            .and_then(|part| part["text"].as_str())
    })
}

/// Whether a tool result's text is the one Claude Code writes for a call the
/// user denied at a permission prompt or interrupted.
pub(crate) fn is_denied_or_interrupted(text: &str) -> bool {
    text.starts_with(DENIED_RESULT_PREFIX) || text.starts_with(INTERRUPTED_PREFIX)
}

/// Each `<task-id>` the task notifications in `text` name, trimmed, in order.
pub(crate) fn task_ids_in(text: &str) -> impl Iterator<Item = &str> {
    tag_values(text, "<task-id>", "</task-id>")
}

/// The trimmed `<status>` of the first task notification in `text`
/// (`completed`, `failed`, `killed`, …); `None` when it carries none.
#[expect(
    dead_code,
    reason = "a shared decoding rule kept beside the task ids it reads with; the activity reader clears a task on any status"
)]
pub(crate) fn task_notification_status(text: &str) -> Option<&str> {
    let first = text
        .split_once("</task-notification>")
        .map_or(text, |(notification, _)| notification);
    tag_values(first, "<status>", "</status>").next()
}

/// The trimmed text between each `open` tag and the `close` tag after it, in
/// order; an `open` with no `close` after it ends the list.
pub(crate) fn tag_values<'a>(
    text: &'a str,
    open: &'static str,
    close: &'static str,
) -> impl Iterator<Item = &'a str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let (_, after) = rest.split_once(open)?;
        let (value, tail) = after.split_once(close)?;
        rest = tail;
        Some(value.trim())
    })
}

/// The id of the background task a `user` record's `toolUseResult` launches:
/// a background shell's `backgroundTaskId`, or the `agentId` of an `Agent`
/// call that went on in the background (`isAsync`, or `status`
/// `async_launched`) or of a skill forked into the background (`status`
/// `forked` with `background`); `None` for any other result.
pub(crate) fn launched_task(result: &Value) -> Option<&str> {
    let non_empty = |field: &str| result[field].as_str().filter(|id| !id.is_empty());
    if let Some(task) = non_empty("backgroundTaskId") {
        return Some(task);
    }
    let async_agent = result["isAsync"] == true || result["status"] == "async_launched";
    let background_fork = result["status"] == "forked" && result["background"] == true;
    if async_agent || background_fork {
        non_empty("agentId")
    } else {
        None
    }
}
