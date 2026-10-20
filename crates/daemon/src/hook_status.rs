//! Maps agent hook payloads onto session status and pending input.
//!
//! Claude Code hooks arrive through the settings file a spawn injects, and an
//! opted-in Codex session through the `-c` hooks table its spawn injects; both
//! shims post to the same endpoint. The daemon's own status watcher
//! (`pty_state`) guesses the status from the byte stream; these hooks are the
//! agent's own report. Reading is tolerant: an unknown event, an unknown field
//! and a body that is not a JSON object are all ignored rather than refused.

use crate::server::Hub;
use crate::session::{PendingInsert, SessionRecord, push_recent_action};
use protocol::{
    AgentOptions, PendingInput, PendingQuestion, SessionMode, SessionStatus, SpawnConfig,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Characters of a plan kept before the text is cut short.
const PLAN_MAX_CHARS: usize = 2000;

/// Characters of a serialised tool input kept as a permission summary.
const DETAIL_MAX_CHARS: usize = 200;

/// Bytes of a `Stop`'s last assistant message kept on the record.
const LAST_MESSAGE_MAX_BYTES: usize = 8 * 1024;

/// How long a hook event waits for the session it names to be inserted. Past
/// it the event is dropped: the session either spawned long ago or never will.
const PENDING_HOOK_TTL: Duration = Duration::from_secs(30);

/// Most events held for one session id while it is being spawned.
const PENDING_HOOK_MAX_PER_SESSION: usize = 32;

/// Most session ids whose early events are held at once.
const PENDING_HOOK_MAX_SESSIONS: usize = 64;

/// The payload fields a mapped event carries beside what [`map_event`] turns
/// into a [`HookSignal`].
#[derive(Debug, Clone, PartialEq)]
pub struct HookDetails {
    /// The event's `hook_event_name`, or empty when it names none.
    pub event: String,
    /// Claude's conversation id for the session.
    pub session_id: Option<String>,
    /// The assistant message a `Stop` ended the turn with.
    pub last_assistant_message: Option<String>,
    /// The CLI's own permission mode at the time of the event.
    pub permission_mode: Option<String>,
    /// Why a `StopFailure` failed: the payload's `reason`, else its `message`.
    pub failure_detail: Option<String>,
}

/// Read the extra fields out of one hook payload.
///
/// Tolerant like [`map_event`]: every field is optional, and one of the wrong
/// JSON type reads as absent. Never panics.
#[must_use]
pub fn details(body: &Value) -> HookDetails {
    let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
    HookDetails {
        event: text("hook_event_name").unwrap_or_default(),
        session_id: text("session_id"),
        last_assistant_message: text("last_assistant_message"),
        permission_mode: text("permission_mode"),
        failure_detail: text("reason")
            .filter(|reason| !reason.is_empty())
            .or_else(|| text("message")),
    }
}

/// `text` cut to at most `max_bytes` UTF-8 bytes on a character boundary,
/// ending in `…`, which counts inside the cap — the same convention as
/// [`truncate`]. Text within the cap is returned unchanged. A cap too small
/// for the ellipsis keeps that many leading bytes and appends nothing.
#[must_use]
pub fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let ellipsis = '…'.len_utf8();
    let mut end = if max_bytes < ellipsis {
        max_bytes
    } else {
        max_bytes - ellipsis
    };
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = text[..end].to_owned();
    if max_bytes >= ellipsis {
        cut.push('…');
    }
    cut
}

/// The spawn's permission mode and the hook's report, when they differ.
///
/// Only a Claude spawn that passed `--permission-mode` states an expected
/// mode: a spawn with no flag, or one running with
/// `--dangerously-skip-permissions`, has nothing to compare against. The
/// CLI's `"auto"` is a mode the daemon never passes, so it never mismatches.
#[must_use]
pub fn permission_mismatch(
    spawn: &SpawnConfig,
    reported: Option<&str>,
) -> Option<(String, String)> {
    if spawn.dangerously_skip_permissions {
        return None;
    }
    let AgentOptions::Claude {
        permission_mode: Some(mode),
        ..
    } = &spawn.agent_options
    else {
        return None;
    };
    let reported = reported?;
    if reported == "auto" || reported == mode.as_cli_arg() {
        return None;
    }
    Some((mode.as_cli_arg().to_owned(), reported.to_owned()))
}

/// What one hook event says about its session.
#[derive(Debug, Clone, PartialEq)]
pub struct HookSignal {
    /// The status the event implies, or `None` when it says nothing about
    /// the status.
    pub status: Option<SessionStatus>,
    /// How the event changes what the session is waiting for.
    pub pending: PendingUpdate,
}

/// How a hook event changes a session's pending input.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingUpdate {
    /// Drop what is pending.
    Clear,
    /// Replace what is pending.
    Set(PendingInput),
    /// Keep what is pending; when nothing is pending, use this.
    KeepOr(PendingInput),
}

/// Map one hook payload onto what it says about its session.
///
/// Tolerant by design: an unknown event, a body that is not a JSON object and
/// a body with no `hook_event_name` all answer `None`, and unknown fields are
/// ignored. Never panics.
#[must_use]
pub fn map_event(body: &Value) -> Option<HookSignal> {
    let event = body.get("hook_event_name")?.as_str()?;
    match event {
        "SessionStart" => Some(HookSignal {
            status: session_start_status(body),
            pending: PendingUpdate::Clear,
        }),
        "SessionEnd" => Some(signal(None, PendingUpdate::Clear)),
        "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" => {
            Some(signal(Some(SessionStatus::Working), PendingUpdate::Clear))
        }
        "PreToolUse" => Some(pre_tool_use(body)),
        "PermissionRequest" => permission_request(body),
        "Notification" => notification(body),
        "Elicitation" => Some(signal(
            Some(SessionStatus::AwaitingInput),
            PendingUpdate::Set(PendingInput::Other {
                message: elicitation_message(body),
            }),
        )),
        "Stop" | "StopFailure" => Some(signal(Some(SessionStatus::Idle), PendingUpdate::Clear)),
        _ => None,
    }
}

/// A signal with `status` and `pending`, for the rows that need no field of
/// their own.
fn signal(status: Option<SessionStatus>, pending: PendingUpdate) -> HookSignal {
    HookSignal { status, pending }
}

/// Apply one mapped hook event to its session.
///
/// `event_time` is the event's `X-RT-Hook-Time`, nanoseconds since the Unix
/// epoch. The ordering check, the stamp and the state writes happen in one
/// registry update, so two concurrent events cannot stamp and then apply out
/// of order. An event at or before the last applied one, or one with no time
/// at all, changes nothing. An unmapped or malformed event never reaches
/// here, so it neither stamps nor drops anything.
///
/// A session that has stopped or errored keeps that status — a hook event
/// still in flight when the session quit must not revive it — but the stamp
/// still advances, so an even older event cannot apply either. The event
/// still marks the session hook-driven: hooks own its promotions from its
/// first applied event on.
///
/// The status moves through [`crate::pty_state::apply_status`], the same
/// helper the PTY watcher's transition uses, inside the same registry update
/// as the ordering check and the `Stopped | Error` guard. One update for the
/// whole event is what keeps two concurrent events in order and stops a
/// terminal status written by the exit path from being revived. `status_since`
/// and the sidecar still come from the registry update; the `Attention` on
/// entry to `AwaitingInput` is sent once the update has returned.
///
/// `details` carries the payload fields the signal itself does not: the
/// conversation id a `SessionStart` reports, a `Stop`'s last message, a
/// `StopFailure`'s reason and a `UserPromptSubmit`'s permission mode. They
/// are written under the same update as the signal, after the ordering check
/// and the terminal guard.
pub fn apply_signal(
    hub: &Hub,
    session_id: &str,
    event_time: Option<u128>,
    signal: &HookSignal,
    details: &HookDetails,
) {
    let mut entered_awaiting = false;
    let mut mismatch = None;
    hub.sessions.update(session_id, |record| {
        (entered_awaiting, mismatch) = apply_to_record(record, event_time, signal, details);
    });
    log_mismatch(session_id, mismatch.as_ref());
    if entered_awaiting {
        crate::pty_state::announce_awaiting_input(session_id, &hub.attention_tx);
    }
}

/// Apply one mapped event to `record`: the ordering check, the stamp and the
/// state writes, as [`apply_signal`] and the pending-event replay both need.
/// Answers whether the record entered `AwaitingInput`, and the permission
/// mismatch the event reported.
pub(crate) fn apply_to_record(
    record: &mut SessionRecord,
    event_time: Option<u128>,
    signal: &HookSignal,
    details: &HookDetails,
) -> (bool, Option<(String, String)>) {
    let Some(event_time) = event_time else {
        return (false, None);
    };
    if record.last_hook_time.is_some_and(|last| event_time <= last) {
        return (false, None);
    }
    record.last_hook_time = Some(event_time);
    record.hook_driven = true;
    if matches!(record.status, SessionStatus::Stopped | SessionStatus::Error) {
        return (false, None);
    }
    // A shell outlives the CLI whose hooks drive it: once that CLI ends, its
    // hooks stop arriving and the shell classifier owns the status again. The
    // status itself is left alone; the plain-shell silence demotion ends a
    // stale `Working`.
    if details.event == "SessionEnd" && record.mode == SessionMode::PlainShell {
        record.hook_driven = false;
    }
    let entered_awaiting = signal
        .status
        .is_some_and(|status| crate::pty_state::apply_status(record, status));
    match &signal.pending {
        PendingUpdate::Clear => record.pending_input = None,
        PendingUpdate::Set(pending) => record.pending_input = Some(pending.clone()),
        PendingUpdate::KeepOr(pending) => {
            if record.pending_input.is_none() {
                record.pending_input = Some(pending.clone());
            }
        }
    }
    (entered_awaiting, apply_details(record, details))
}

/// Warn about a hook-reported permission mode that differs from the spawn's.
fn log_mismatch(session_id: &str, mismatch: Option<&(String, String)>) {
    if let Some((expected, reported)) = mismatch {
        warn!(
            session_id,
            expected, reported, "hook permission mode differs from the spawn's"
        );
    }
}

/// One hook event that arrived before its session was inserted.
#[derive(Debug, Clone)]
struct PendingHook {
    at: Instant,
    event_time: Option<u128>,
    signal: HookSignal,
    details: HookDetails,
}

/// Hook events that arrived for a session id the registry did not hold yet,
/// i.e. a spawn in flight. Bounded by [`PENDING_HOOK_TTL`] and the two count
/// caps; an event is applied to its record when the spawn inserts it and
/// dropped when it expires or a cap evicts it.
#[derive(Clone, Default)]
pub struct PendingHooks(Arc<std::sync::Mutex<HashMap<String, Vec<PendingHook>>>>);

impl PendingHooks {
    /// Hold an event for `session_id` until its spawn inserts the session.
    ///
    /// The buffer lock is also the insert's lock: an event either lands in
    /// the buffer before the insert takes it (and is replayed into the
    /// record), or this call finds the session registered and applies the
    /// event directly. Neither order can drop it.
    pub fn defer(
        &self,
        hub: &Hub,
        session_id: &str,
        event_time: Option<u128>,
        signal: &HookSignal,
        details: &HookDetails,
    ) {
        let mut buffer = crate::sync::lock(&self.0);
        let now = Instant::now();
        prune_expired(&mut buffer, now);
        if hub.sessions.get(session_id).is_some() {
            drop(buffer);
            apply_signal(hub, session_id, event_time, signal, details);
            return;
        }
        if !buffer.contains_key(session_id) && buffer.len() >= PENDING_HOOK_MAX_SESSIONS {
            let oldest = buffer
                .iter()
                .min_by_key(|(_, events)| events.last().map(|event| event.at))
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                buffer.remove(&oldest);
            }
        }
        let events = buffer.entry(session_id.to_owned()).or_default();
        if events.len() >= PENDING_HOOK_MAX_PER_SESSION {
            events.remove(0);
        }
        events.push(PendingHook {
            at: now,
            event_time,
            signal: signal.clone(),
            details: details.clone(),
        });
        debug!(session_id, "hook event held until its session is inserted");
    }

    /// Insert `record` with every held event for its id already applied,
    /// under the buffer lock so a concurrent [`Self::defer`] cannot land
    /// between the take and the insert. Answers whether the record still holds
    /// an `AwaitingInput` prompt after the whole replay, so the caller raises
    /// its one `Attention` after publishing the session: a later event in the
    /// buffer may have cleared the prompt an earlier one set.
    pub fn insert_with_pending<'a>(
        &self,
        hub: &'a Hub,
        mut record: SessionRecord,
    ) -> (PendingInsert<'a>, bool) {
        let session_id = record.id.clone();
        let mut buffer = crate::sync::lock(&self.0);
        prune_expired(&mut buffer, Instant::now());
        let events = buffer.remove(&session_id).unwrap_or_default();
        for event in events {
            let (_, mismatch) =
                apply_to_record(&mut record, event.event_time, &event.signal, &event.details);
            log_mismatch(&session_id, mismatch.as_ref());
        }
        let awaiting =
            record.status == SessionStatus::AwaitingInput && record.pending_input.is_some();
        (hub.sessions.insert_pending(record), awaiting)
    }
}

/// Drop held events older than [`PENDING_HOOK_TTL`], and the ids left with
/// none.
fn prune_expired(buffer: &mut HashMap<String, Vec<PendingHook>>, now: Instant) {
    buffer.retain(|_, events| {
        events.retain(|event| now.saturating_duration_since(event.at) < PENDING_HOOK_TTL);
        !events.is_empty()
    });
}

/// Apply the payload fields the event's signal does not carry, answering the
/// permission mismatch a prompt reported.
pub(crate) fn apply_details(
    record: &mut SessionRecord,
    details: &HookDetails,
) -> Option<(String, String)> {
    match details.event.as_str() {
        "SessionStart" => {
            update_conversation_id(record, details);
            None
        }
        "Stop" => {
            store_last_message(record, details);
            None
        }
        "StopFailure" => {
            record_stop_failure(record, details);
            None
        }
        "UserPromptSubmit" => permission_warning(record, details),
        _ => None,
    }
}

/// The expected and reported modes a prompt's warning names, when they
/// differ. The injector's own intent is not stored, so this compares against
/// the CLI flag the daemon actually sent.
pub(crate) fn permission_warning(
    record: &SessionRecord,
    details: &HookDetails,
) -> Option<(String, String)> {
    let spawn = record.spawn_config.as_ref()?;
    permission_mismatch(spawn, details.permission_mode.as_deref())
}

/// A `SessionStart` on a new conversation moves the record to its id. A
/// resumed one reports the id it already had, so the write is a no-op; a
/// value that is not a plain id is refused, since it later becomes
/// `claude --resume <id>` and a transcript path.
fn update_conversation_id(record: &mut SessionRecord, details: &HookDetails) {
    let Some(id) = details.session_id.as_deref() else {
        return;
    };
    if crate::transcripts::is_plain_id(id) && record.claude_session_id.as_deref() != Some(id) {
        record.claude_session_id = Some(id.to_owned());
    }
}

/// A `Stop` keeps the message the turn ended with. A `Stop` with no message
/// keeps the previous one; a message is capped so it cannot grow the record
/// without bound.
fn store_last_message(record: &mut SessionRecord, details: &HookDetails) {
    let Some(message) = details.last_assistant_message.as_deref() else {
        return;
    };
    let message = message.trim();
    if !message.is_empty() {
        record.last_assistant_message = Some(truncate_bytes(message, LAST_MESSAGE_MAX_BYTES));
    }
}

/// A `StopFailure` leaves one recent-actions line, naming the payload's
/// reason when it carries one.
fn record_stop_failure(record: &mut SessionRecord, details: &HookDetails) {
    let line = match details
        .failure_detail
        .as_deref()
        .filter(|detail| !detail.is_empty())
    {
        Some(detail) => format!("stop failed: {}", truncate(detail, DETAIL_MAX_CHARS)),
        None => "stop failed".to_owned(),
    };
    push_recent_action(record, line);
}

/// `SessionStart` says nothing about the status unless the session starts
/// fresh, so its `Idle` cannot demote a resumed session's prompt.
fn session_start_status(body: &Value) -> Option<SessionStatus> {
    match body.get("source").and_then(Value::as_str) {
        Some("startup") => Some(SessionStatus::Idle),
        _ => None,
    }
}

/// A tool about to run: the two prompt tools say what is being asked, any
/// other tool means the turn is working.
fn pre_tool_use(body: &Value) -> HookSignal {
    let name = tool_name(body).unwrap_or_default();
    let input = body.get("tool_input");
    match name {
        "AskUserQuestion" => signal(
            Some(SessionStatus::AwaitingInput),
            PendingUpdate::Set(PendingInput::Question {
                questions: questions(input),
            }),
        ),
        "ExitPlanMode" => signal(
            Some(SessionStatus::AwaitingInput),
            PendingUpdate::Set(PendingInput::PlanApproval { plan: plan(input) }),
        ),
        _ => signal(Some(SessionStatus::Working), PendingUpdate::Clear),
    }
}

/// A permission prompt, mapped by `tool_name`: it fires for the two prompt
/// tools as well, and a plain `Permission` would overwrite the richer
/// pending input their `PreToolUse` set 90-300 ms earlier.
///
/// A prompt with no tool name is malformed and says nothing: a nameless
/// `Permission` would describe nothing about what is up.
fn permission_request(body: &Value) -> Option<HookSignal> {
    let name = body.get("tool_name").and_then(Value::as_str)?;
    let input = body.get("tool_input");
    let pending = match name {
        "AskUserQuestion" => PendingInput::Question {
            questions: questions(input),
        },
        "ExitPlanMode" => PendingInput::PlanApproval { plan: plan(input) },
        _ => PendingInput::Permission {
            tool_name: name.to_owned(),
            summary: permission_summary(input),
        },
    };
    Some(signal(
        Some(SessionStatus::AwaitingInput),
        PendingUpdate::Set(pending),
    ))
}

/// A notification is the backstop for a prompt `PermissionRequest` misses.
/// Its message counts only when nothing richer is already pending.
fn notification(body: &Value) -> Option<HookSignal> {
    let kind = body.get("notification_type").and_then(Value::as_str)?;
    if kind == "idle_prompt" {
        return Some(signal(Some(SessionStatus::Idle), PendingUpdate::Clear));
    }
    if kind == "permission_prompt" || kind.starts_with("elicitation_") {
        // Neither kind's `message` is guaranteed, so each falls back to what
        // the prompt is.
        let fallback = if kind == "permission_prompt" {
            "Claude Code asks for permission"
        } else {
            "An MCP server asks for input"
        };
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(fallback);
        return Some(signal(
            Some(SessionStatus::AwaitingInput),
            PendingUpdate::KeepOr(PendingInput::Other {
                message: message.to_owned(),
            }),
        ));
    }
    None
}

/// The tool a payload names, when it names one.
fn tool_name(body: &Value) -> Option<&str> {
    body.get("tool_name").and_then(Value::as_str)
}

/// The questions an `AskUserQuestion` payload carries; empty when it carries
/// none.
fn questions(input: Option<&Value>) -> Vec<PendingQuestion> {
    input
        .and_then(|input| input.get("questions"))
        .and_then(Value::as_array)
        .map(|items| items.iter().map(question).collect())
        .unwrap_or_default()
}

/// One question, with what the prompt left out filled in.
fn question(item: &Value) -> PendingQuestion {
    PendingQuestion {
        question: text(item, "question"),
        header: text(item, "header"),
        options: item
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| option.get("label").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        multi_select: item
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The plan an `ExitPlanMode` call carries, when it carries one.
fn plan(input: Option<&Value>) -> Option<String> {
    let raw = input?.get("plan")?.as_str()?;
    Some(truncate(raw, PLAN_MAX_CHARS))
}

/// What a permission prompt asks for: the command, the file, or the
/// serialised input when it has neither. Every form is capped, so a command
/// thousands of characters long does not ride every snapshot.
fn permission_summary(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let asked = input
        .get("command")
        .and_then(Value::as_str)
        .or_else(|| input.get("file_path").and_then(Value::as_str))
        .map_or_else(|| input.to_string(), str::to_owned);
    truncate(&asked, DETAIL_MAX_CHARS)
}

/// An MCP elicitation's message, named by its server.
fn elicitation_message(body: &Value) -> String {
    let server = body
        .get("mcp_server_name")
        .and_then(Value::as_str)
        .unwrap_or("An MCP server");
    match body.get("message").and_then(Value::as_str) {
        Some(message) => format!("{server}: {message}"),
        None => format!("{server} asks for input"),
    }
}

/// `text` cut to at most `max` characters, the trailing `…` counted inside
/// them when it was longer — the same convention as the transcript title's
/// cut in [`crate::transcripts`].
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use protocol::{PermissionMode, SessionMode, SpawnTarget};
    use serde_json::json;

    /// The signal `body` maps to.
    fn signal(body: &Value) -> HookSignal {
        map_event(body).expect("the payload maps to a signal")
    }

    /// The status `body` implies.
    fn status_of(body: &Value) -> Option<SessionStatus> {
        signal(body).status
    }

    /// How `body` changes the pending input.
    fn pending_of(body: &Value) -> PendingUpdate {
        signal(body).pending
    }

    /// A permission prompt's summary.
    fn summary_of(body: &Value) -> String {
        match signal(body).pending {
            PendingUpdate::Set(PendingInput::Permission { summary, .. }) => summary,
            other => format!("not a permission prompt: {other:?}"),
        }
    }

    #[test]
    fn session_start_is_idle_only_on_a_startup() {
        let startup = json!({"hook_event_name": "SessionStart", "source": "startup"});
        assert_eq!(status_of(&startup), Some(SessionStatus::Idle));
        assert_eq!(pending_of(&startup), PendingUpdate::Clear);
        for source in ["resume", "clear", "compact", "fork"] {
            assert_eq!(
                status_of(&json!({"hook_event_name": "SessionStart", "source": source})),
                None,
                "a {source} leaves the status alone"
            );
        }
        assert_eq!(status_of(&json!({"hook_event_name": "SessionStart"})), None);
    }

    #[test]
    fn a_new_prompt_or_a_finished_tool_means_working() {
        for event in ["UserPromptSubmit", "PostToolUse", "PostToolUseFailure"] {
            assert_eq!(
                status_of(&json!({"hook_event_name": event, "prompt": "say hi"})),
                Some(SessionStatus::Working),
                "{event}"
            );
            assert_eq!(
                pending_of(&json!({"hook_event_name": event})),
                PendingUpdate::Clear
            );
        }
        assert_eq!(
            status_of(&json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "ls"},
            })),
            Some(SessionStatus::Working)
        );
    }

    #[test]
    fn a_question_tool_carries_its_questions() {
        let body = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "AskUserQuestion",
            "tool_input": {"questions": [
                {
                    "question": "Do you prefer tea or coffee?",
                    "header": "Drink",
                    "options": [
                        {"label": "Tea", "description": "You prefer tea."},
                        {"label": "Coffee", "description": "You prefer coffee."}
                    ],
                    "multiSelect": true
                },
                {"question": "Which editor?", "options": [{"label": "vim"}]}
            ]},
        });
        assert_eq!(status_of(&body), Some(SessionStatus::AwaitingInput));
        assert_eq!(
            pending_of(&body),
            PendingUpdate::Set(PendingInput::Question {
                questions: vec![
                    PendingQuestion {
                        question: "Do you prefer tea or coffee?".to_owned(),
                        header: "Drink".to_owned(),
                        options: vec!["Tea".to_owned(), "Coffee".to_owned()],
                        multi_select: true,
                    },
                    PendingQuestion {
                        question: "Which editor?".to_owned(),
                        header: String::new(),
                        options: vec!["vim".to_owned()],
                        multi_select: false,
                    },
                ],
            })
        );
    }

    #[test]
    fn a_plan_approval_carries_the_plan_it_has() {
        assert_eq!(
            pending_of(&json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "ExitPlanMode",
                "tool_input": {
                    "plan": "1. Print toasty\n",
                    "planFilePath": "C:\\plans\\plan-toasty.md",
                },
            })),
            PendingUpdate::Set(PendingInput::PlanApproval {
                plan: Some("1. Print toasty\n".to_owned()),
            })
        );
        // The model can call the tool with no plan written: `tool_input` is
        // then `{}` and the approval carries none.
        let empty = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "ExitPlanMode",
            "tool_input": {},
        });
        assert_eq!(status_of(&empty), Some(SessionStatus::AwaitingInput));
        assert_eq!(
            pending_of(&empty),
            PendingUpdate::Set(PendingInput::PlanApproval { plan: None })
        );
    }

    #[test]
    fn a_permission_request_is_mapped_by_tool_name() {
        // `PermissionRequest` fires for the two prompt tools too, 90-300 ms
        // after their `PreToolUse`, with the same `tool_input`.
        assert_eq!(
            pending_of(&json!({
                "hook_event_name": "PermissionRequest",
                "tool_name": "AskUserQuestion",
                "tool_input": {"questions": [{"question": "Tea or coffee?"}]},
            })),
            PendingUpdate::Set(PendingInput::Question {
                questions: vec![PendingQuestion {
                    question: "Tea or coffee?".to_owned(),
                    header: String::new(),
                    options: Vec::new(),
                    multi_select: false,
                }],
            })
        );
        assert_eq!(
            pending_of(&json!({
                "hook_event_name": "PermissionRequest",
                "tool_name": "ExitPlanMode",
                "tool_input": {"plan": "1. Print toasty"},
            })),
            PendingUpdate::Set(PendingInput::PlanApproval {
                plan: Some("1. Print toasty".to_owned()),
            })
        );
        let bash = json!({
            "hook_event_name": "PermissionRequest",
            "permission_mode": "default",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi > out.txt", "description": "Write hi"},
            "permission_suggestions": [{"type": "addDirectories"}],
        });
        assert_eq!(status_of(&bash), Some(SessionStatus::AwaitingInput));
        assert_eq!(
            pending_of(&bash),
            PendingUpdate::Set(PendingInput::Permission {
                tool_name: "Bash".to_owned(),
                summary: "echo hi > out.txt".to_owned(),
            })
        );
    }

    #[test]
    fn a_permission_summary_names_the_command_or_the_file() {
        assert_eq!(
            summary_of(&json!({
                "hook_event_name": "PermissionRequest",
                "tool_name": "Write",
                "tool_input": {"file_path": "D:\\x\\out.txt", "content": "hi"},
            })),
            "D:\\x\\out.txt"
        );
        // Neither a command nor a file: the serialised input stands in.
        assert_eq!(
            summary_of(&json!({
                "hook_event_name": "PermissionRequest",
                "tool_name": "WebFetch",
                "tool_input": {"url": "https://example.com"},
            })),
            r#"{"url":"https://example.com"}"#
        );
        let long = "y".repeat(300);
        let summary = summary_of(&json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "WebFetch",
            "tool_input": {"url": long},
        }));
        assert_eq!(summary.chars().count(), DETAIL_MAX_CHARS, "{summary}");
        assert!(summary.starts_with(r#"{"url":"yyy"#), "{summary}");
        assert!(summary.ends_with('…'), "{summary}");
        // A command and a file path are capped the same way: a heredoc
        // thousands of characters long must not ride every snapshot.
        let command = summary_of(&json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": {"command": "z".repeat(5000)},
        }));
        assert_eq!(command, format!("{}…", "z".repeat(DETAIL_MAX_CHARS - 1)));
        let path = summary_of(&json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "Write",
            "tool_input": {"file_path": format!("D:\\{}", "p".repeat(5000))},
        }));
        assert_eq!(path.chars().count(), DETAIL_MAX_CHARS, "{path}");
        assert!(path.ends_with('…'), "{path}");
    }

    #[test]
    fn a_permission_request_without_a_tool_name_is_malformed() {
        assert!(
            map_event(&json!({"hook_event_name": "PermissionRequest", "tool_input": {}})).is_none()
        );
        assert!(
            map_event(&json!({
                "hook_event_name": "PermissionRequest",
                "tool_name": 7,
            }))
            .is_none()
        );
        // A `PreToolUse` without one is just an unnamed tool: the turn works.
        assert_eq!(
            status_of(&json!({"hook_event_name": "PreToolUse"})),
            Some(SessionStatus::Working)
        );
        assert_eq!(
            status_of(&json!({"hook_event_name": "PreToolUse", "tool_name": 7})),
            Some(SessionStatus::Working)
        );
    }

    #[test]
    fn a_prompt_notification_is_a_backstop_that_keeps_a_richer_answer() {
        let permission = json!({
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "Claude needs your permission",
        });
        assert_eq!(status_of(&permission), Some(SessionStatus::AwaitingInput));
        assert_eq!(
            pending_of(&permission),
            PendingUpdate::KeepOr(PendingInput::Other {
                message: "Claude needs your permission".to_owned(),
            })
        );
        // A notification's message is optional: each kind falls back to what
        // the prompt is, so the pending input never shows an empty line.
        assert_eq!(
            pending_of(&json!({
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt",
            })),
            PendingUpdate::KeepOr(PendingInput::Other {
                message: "Claude Code asks for permission".to_owned(),
            })
        );
        assert_eq!(
            pending_of(&json!({
                "hook_event_name": "Notification",
                "notification_type": "elicitation_dialog",
            })),
            PendingUpdate::KeepOr(PendingInput::Other {
                message: "An MCP server asks for input".to_owned(),
            })
        );
        assert_eq!(
            status_of(&json!({
                "hook_event_name": "Notification",
                "notification_type": "elicitation_url_dialog",
                "message": "An MCP server wants input",
            })),
            Some(SessionStatus::AwaitingInput)
        );
    }

    #[test]
    fn an_idle_notification_is_idle_and_other_kinds_say_nothing() {
        let idle = json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"});
        assert_eq!(status_of(&idle), Some(SessionStatus::Idle));
        assert_eq!(pending_of(&idle), PendingUpdate::Clear);
        assert!(
            map_event(&json!({
                "hook_event_name": "Notification",
                "notification_type": "something_else",
            }))
            .is_none()
        );
        assert!(map_event(&json!({"hook_event_name": "Notification"})).is_none());
    }

    #[test]
    fn an_elicitation_names_the_mcp_server() {
        assert_eq!(
            pending_of(&json!({
                "hook_event_name": "Elicitation",
                "mcp_server_name": "linear",
                "message": "which issue?",
            })),
            PendingUpdate::Set(PendingInput::Other {
                message: "linear: which issue?".to_owned(),
            })
        );
        let named = json!({"hook_event_name": "Elicitation", "mcp_server_name": "linear"});
        assert_eq!(status_of(&named), Some(SessionStatus::AwaitingInput));
        assert_eq!(
            pending_of(&named),
            PendingUpdate::Set(PendingInput::Other {
                message: "linear asks for input".to_owned(),
            })
        );
        assert_eq!(
            pending_of(&json!({"hook_event_name": "Elicitation"})),
            PendingUpdate::Set(PendingInput::Other {
                message: "An MCP server asks for input".to_owned(),
            })
        );
    }

    #[test]
    fn details_reads_each_field() {
        let body = json!({
            "hook_event_name": "StopFailure",
            "session_id": "0b6f4c7e-1111-4222-8333-944455556666",
            "last_assistant_message": "Done.",
            "permission_mode": "plan",
            "reason": "overloaded",
            "message": "ignored",
        });
        assert_eq!(
            details(&body),
            HookDetails {
                event: "StopFailure".to_owned(),
                session_id: Some("0b6f4c7e-1111-4222-8333-944455556666".to_owned()),
                last_assistant_message: Some("Done.".to_owned()),
                permission_mode: Some("plan".to_owned()),
                failure_detail: Some("overloaded".to_owned()),
            }
        );
    }

    #[test]
    fn details_tolerates_missing_fields() {
        let missing = json!({"hook_event_name": "Stop"});
        assert_eq!(
            details(&missing),
            HookDetails {
                event: "Stop".to_owned(),
                session_id: None,
                last_assistant_message: None,
                permission_mode: None,
                failure_detail: None,
            }
        );
        // A wrong-typed field reads as absent, and `message` stands in for a
        // missing or non-string `reason`.
        let typed = json!({"session_id": 7, "reason": 3, "message": "boom"});
        let read = details(&typed);
        assert_eq!(read.event, "");
        assert_eq!(read.session_id, None);
        assert_eq!(read.failure_detail.as_deref(), Some("boom"));
    }

    #[test]
    fn truncate_bytes_cuts_on_a_char_boundary() {
        // 10 bytes, with the two-byte `é` straddling bytes 5 and 6, so the
        // cap's first landing point is inside a character.
        let text = "aaaaaéZZZ";
        let cut = truncate_bytes(text, 9);
        assert_eq!(cut, "aaaaa…");
        assert!(cut.len() <= 9, "{cut:?} is {} bytes", cut.len());
        // The `…` counts inside the cap, like `truncate`.
        assert_eq!(truncate_bytes(text, 8), "aaaaa…");
    }

    #[test]
    fn truncate_bytes_keeps_short_text() {
        assert_eq!(truncate_bytes("hello", LAST_MESSAGE_MAX_BYTES), "hello");
        assert_eq!(truncate_bytes("", 0), "");
        let at_cap = "b".repeat(LAST_MESSAGE_MAX_BYTES);
        assert_eq!(truncate_bytes(&at_cap, LAST_MESSAGE_MAX_BYTES), at_cap);
    }

    #[test]
    fn truncate_bytes_below_the_ellipsis_keeps_the_cap() {
        assert_eq!(truncate_bytes("abcd", 0), "");
        assert_eq!(truncate_bytes("abcd", 1), "a");
        assert_eq!(truncate_bytes("abcd", 2), "ab");
        // A cap this small still lands on a character boundary.
        assert_eq!(truncate_bytes("éabcd", 1), "");
        assert_eq!(truncate_bytes("éabcd", 2), "é");
    }

    #[test]
    fn details_falls_back_from_an_empty_reason_to_the_message() {
        let body = json!({
            "hook_event_name": "StopFailure",
            "reason": "",
            "message": "boom",
        });
        assert_eq!(details(&body).failure_detail.as_deref(), Some("boom"));
        let neither = json!({"hook_event_name": "StopFailure", "reason": ""});
        assert_eq!(details(&neither).failure_detail, None);
    }

    /// The prompt route answers the pair `apply_signal` warns about, from the
    /// payload and the spawn's flag.
    #[test]
    fn user_prompt_submit_answers_the_permission_mismatch() {
        let mut rec = crate::history::test_support::record("s1", SessionMode::Interactive);
        rec.spawn_config = Some(spawn_with(Some(PermissionMode::Plan), false));
        let differing = json!({
            "hook_event_name": "UserPromptSubmit",
            "permission_mode": "default",
        });
        assert_eq!(
            apply_details(&mut rec, &details(&differing)),
            Some(("plan".to_owned(), "default".to_owned()))
        );
        let matching = json!({
            "hook_event_name": "UserPromptSubmit",
            "permission_mode": "plan",
        });
        assert_eq!(apply_details(&mut rec, &details(&matching)), None);
    }

    /// A spawn that passed `mode` as `--permission-mode`, or none.
    fn spawn_with(mode: Option<PermissionMode>, yolo: bool) -> SpawnConfig {
        SpawnConfig {
            target: SpawnTarget::Standalone {
                cwd: None,
                add_dirs: Vec::new(),
            },
            mode: SessionMode::Interactive,
            dangerously_skip_permissions: yolo,
            agent_options: AgentOptions::claude(mode),
            model: None,
            extra_env: Vec::new(),
        }
    }

    #[test]
    fn permission_mismatch_reports_a_different_mode() {
        let spawn = spawn_with(Some(PermissionMode::Plan), false);
        assert_eq!(
            permission_mismatch(&spawn, Some("default")),
            Some(("plan".to_owned(), "default".to_owned()))
        );
        assert_eq!(permission_mismatch(&spawn, Some("plan")), None);
    }

    #[test]
    fn permission_mismatch_ignores_auto_and_missing_reports() {
        let spawn = spawn_with(Some(PermissionMode::Plan), false);
        assert_eq!(permission_mismatch(&spawn, Some("auto")), None);
        assert_eq!(permission_mismatch(&spawn, None), None);
    }

    #[test]
    fn permission_mismatch_needs_a_flagged_spawn() {
        assert_eq!(
            permission_mismatch(&spawn_with(None, false), Some("plan")),
            None,
            "no --permission-mode means nothing to compare against"
        );
        assert_eq!(
            permission_mismatch(
                &spawn_with(Some(PermissionMode::Plan), true),
                Some("default")
            ),
            None,
            "yolo passes no flag either"
        );
        let codex = SpawnConfig {
            agent_options: AgentOptions::Codex {
                sandbox: None,
                hooks: false,
            },
            ..spawn_with(Some(PermissionMode::Plan), false)
        };
        assert_eq!(permission_mismatch(&codex, Some("plan")), None);
    }

    #[test]
    fn a_finished_turn_is_idle_and_an_ended_session_leaves_the_status() {
        for event in ["Stop", "StopFailure"] {
            assert_eq!(
                status_of(&json!({"hook_event_name": event, "last_assistant_message": "Done."})),
                Some(SessionStatus::Idle),
                "{event}"
            );
            assert_eq!(
                pending_of(&json!({"hook_event_name": event})),
                PendingUpdate::Clear
            );
        }
        let end = json!({"hook_event_name": "SessionEnd", "reason": "prompt_input_exit"});
        assert_eq!(status_of(&end), None);
        assert_eq!(pending_of(&end), PendingUpdate::Clear);
    }

    #[test]
    fn a_body_that_names_no_known_hook_says_nothing() {
        assert!(map_event(&json!({"hook_event_name": "SomethingNew"})).is_none());
        assert!(map_event(&json!(["an", "array"])).is_none());
        assert!(map_event(&json!("a string")).is_none());
        assert!(map_event(&json!({"tool_name": "Bash"})).is_none());
        assert!(map_event(&json!({"hook_event_name": 7})).is_none());
    }

    #[test]
    fn a_long_plan_is_cut_at_two_thousand_characters() {
        // The cap counts the `…` it ends with, so a 2,000-character plan
        // comes out as 2,000 characters, ellipsis included.
        let long = "x".repeat(PLAN_MAX_CHARS + 500);
        assert_eq!(
            pending_of(&json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "ExitPlanMode",
                "tool_input": {"plan": long},
            })),
            PendingUpdate::Set(PendingInput::PlanApproval {
                plan: Some(format!("{}…", "x".repeat(PLAN_MAX_CHARS - 1))),
            })
        );
        // A plan exactly at the cap is not cut.
        assert_eq!(
            pending_of(&json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "ExitPlanMode",
                "tool_input": {"plan": "x".repeat(PLAN_MAX_CHARS)},
            })),
            PendingUpdate::Set(PendingInput::PlanApproval {
                plan: Some("x".repeat(PLAN_MAX_CHARS)),
            })
        );
    }
}
