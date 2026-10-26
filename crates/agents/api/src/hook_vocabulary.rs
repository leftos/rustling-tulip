//! Claude Code's hook vocabulary: its event names and payload fields, mapped
//! onto a [`HookReport`].
//!
//! Any agent whose hooks speak this vocabulary calls [`map`] instead of
//! copying it. Reading is tolerant: an unknown event, an unknown field and a
//! body that is not a JSON object are all ignored rather than refused.

use crate::{HookReport, PendingUpdate};
use protocol::{PendingInput, PendingQuestion, SessionStatus};
use serde_json::Value;

/// Characters of a plan kept before the text is cut short.
const PLAN_MAX_CHARS: usize = 2000;

/// Characters of a serialised tool input kept as a permission summary, and
/// of a failure reason kept in its recent-actions line.
const DETAIL_MAX_CHARS: usize = 200;

/// Characters kept of a question's text and of an `Other` prompt's message.
const MESSAGE_MAX_CHARS: usize = 1000;

/// Questions kept from one `AskUserQuestion` payload.
const MAX_QUESTIONS: usize = 8;

/// Option labels kept per question.
const MAX_OPTIONS: usize = 8;

/// The payload fields a mapped event carries beside its status and pending
/// input.
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
    /// The payload's `reason`: why a `SessionEnd` ended (`clear` for a
    /// `/clear`, which keeps the agent running).
    pub reason: Option<String>,
    /// Whether a subagent's tool call fired the event: its input carries a
    /// non-empty `agent_id`. (`agent_type` alone also marks a main agent run
    /// with `--agent`, so it is not the marker.)
    pub subagent: bool,
    /// Whether the payload reports background work: a `background_tasks`
    /// element with `"status": "running"`, or a `session_crons` element with
    /// `"recurring": false` (a pending one-shot wakeup). Missing keys,
    /// non-arrays and malformed elements count as none.
    pub background_work: bool,
}

/// Read the extra fields out of one hook payload.
///
/// Tolerant like [`map`]: every field is optional, and one of the wrong JSON
/// type reads as absent. Never panics.
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
        reason: text("reason"),
        subagent: text("agent_id").is_some_and(|id| !id.is_empty()),
        background_work: reports_background_work(body),
    }
}

/// Whether `body` lists a running background task or a pending one-shot
/// cron. A recurring cron alone is not background work.
fn reports_background_work(body: &Value) -> bool {
    let array = |key: &str| body.get(key).and_then(Value::as_array);
    let running_task = array("background_tasks").is_some_and(|tasks| {
        tasks
            .iter()
            .any(|task| task.get("status").and_then(Value::as_str) == Some("running"))
    });
    let one_shot_cron = array("session_crons").is_some_and(|crons| {
        crons
            .iter()
            .any(|cron| cron.get("recurring").and_then(Value::as_bool) == Some(false))
    });
    running_task || one_shot_cron
}

/// Map one hook payload onto what it says about its session.
///
/// Tolerant by design: an unknown event, a body that is not a JSON object and
/// a body with no `hook_event_name` all answer `None`, and unknown fields are
/// ignored. Never panics. The report names no permission mismatch: only the
/// agent knows the mode its spawn asked for.
#[must_use]
pub fn map(body: &Value) -> Option<HookReport> {
    let signal = map_signal(body)?;
    Some(report(signal, &details(body)))
}

/// The report of an event whose status and pending input are `signal`, with
/// the rest read from its `details`.
fn report(signal: Signal, details: &HookDetails) -> HookReport {
    let event = details.event.as_str();
    let failure = (event == "StopFailure").then(|| {
        match details
            .failure_detail
            .as_deref()
            .filter(|detail| !detail.is_empty())
        {
            Some(detail) => format!("stop failed: {}", truncate(detail, DETAIL_MAX_CHARS)),
            None => "stop failed".to_owned(),
        }
    });
    HookReport {
        weak_idle: event == "Notification" && signal.status == Some(SessionStatus::Idle),
        status: signal.status,
        pending: signal.pending,
        conversation_id: details
            .session_id
            .clone()
            .filter(|_| event == "SessionStart"),
        last_message: details
            .last_assistant_message
            .clone()
            .filter(|_| event == "Stop"),
        failure,
        permission_mismatch: None,
        agent_ended: event == "SessionEnd" && details.reason.as_deref() != Some("clear"),
        agent_started: event == "SessionStart",
        subagent: details.subagent,
        background: (event == "Stop" && !details.subagent).then_some(details.background_work),
    }
}

/// What one hook event says about its session's status and pending input.
struct Signal {
    status: Option<SessionStatus>,
    pending: PendingUpdate,
}

/// The status and pending-input change one hook payload implies.
fn map_signal(body: &Value) -> Option<Signal> {
    let event = body.get("hook_event_name")?.as_str()?;
    match event {
        "SessionStart" => Some(Signal {
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
fn signal(status: Option<SessionStatus>, pending: PendingUpdate) -> Signal {
    Signal { status, pending }
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
fn pre_tool_use(body: &Value) -> Signal {
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
fn permission_request(body: &Value) -> Option<Signal> {
    let name = body.get("tool_name").and_then(Value::as_str)?;
    let input = body.get("tool_input");
    let pending = match name {
        "AskUserQuestion" => PendingInput::Question {
            questions: questions(input),
        },
        "ExitPlanMode" => PendingInput::PlanApproval { plan: plan(input) },
        _ => PendingInput::Permission {
            tool_name: truncate(name, DETAIL_MAX_CHARS),
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
fn notification(body: &Value) -> Option<Signal> {
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
                message: truncate(message, MESSAGE_MAX_CHARS),
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
        .map(|items| items.iter().take(MAX_QUESTIONS).map(question).collect())
        .unwrap_or_default()
}

/// One question, with what the prompt left out filled in.
fn question(item: &Value) -> PendingQuestion {
    PendingQuestion {
        question: truncate(&text(item, "question"), MESSAGE_MAX_CHARS),
        header: truncate(&text(item, "header"), DETAIL_MAX_CHARS),
        options: item
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| option.get("label").and_then(Value::as_str))
                    .take(MAX_OPTIONS)
                    .map(|label| truncate(label, DETAIL_MAX_CHARS))
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
    if let Some(command) = input.get("command").and_then(Value::as_str) {
        return truncate(command, DETAIL_MAX_CHARS);
    }
    if let Some(path) = input.get("file_path").and_then(Value::as_str) {
        return truncate(path, DETAIL_MAX_CHARS);
    }
    json_prefix(input, DETAIL_MAX_CHARS)
}

/// `value` serialised as JSON and cut like [`truncate`] to `max` characters,
/// without serialising more of it than the cut keeps.
fn json_prefix(value: &Value, max: usize) -> String {
    let mut buf = CappedBuf {
        bytes: Vec::new(),
        limit: max * 4 + 4,
    };
    // The error is the stop signal once the buffer is full, not a failure.
    let _ = serde_json::to_writer(&mut buf, value);
    let text = match std::str::from_utf8(&buf.bytes) {
        Ok(text) => text,
        // The cut lands inside a multi-byte char only at the very end, and
        // every byte before it is exactly the prefix that had been written.
        Err(err) => std::str::from_utf8(&buf.bytes[..err.valid_up_to()]).unwrap_or(""),
    };
    truncate(text, max)
}

/// A byte sink that keeps at most `limit` bytes and then refuses the write,
/// which is how [`serde_json::to_writer`] is stopped early.
struct CappedBuf {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for CappedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let room = self.limit.saturating_sub(self.bytes.len());
        if room == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        let taken = data.len().min(room);
        self.bytes.extend_from_slice(&data[..taken]);
        if taken < data.len() {
            // Full: refusing the rest is how the serializer is told to stop.
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        Ok(taken)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An MCP elicitation's message, named by its server.
fn elicitation_message(body: &Value) -> String {
    let server = body
        .get("mcp_server_name")
        .and_then(Value::as_str)
        .unwrap_or("An MCP server");
    let message = match body.get("message").and_then(Value::as_str) {
        Some(message) => format!("{server}: {message}"),
        None => format!("{server} asks for input"),
    };
    truncate(&message, MESSAGE_MAX_CHARS)
}

/// `text` cut to at most `max` characters, the trailing `…` counted inside
/// them when it was longer — the same convention as the transcript title's
/// cut in agent-claude's `transcripts` module.
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
    use serde_json::json;

    /// `body` as [`map`] reports it.
    fn map_event(body: &Value) -> Option<HookReport> {
        map(body)
    }

    /// The report `body` maps to.
    fn signal(body: &Value) -> HookReport {
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

    /// The questions a question prompt carries, when the payload maps to one.
    fn questions_of(body: &Value) -> Option<Vec<PendingQuestion>> {
        match pending_of(body) {
            PendingUpdate::Set(PendingInput::Question { questions }) => Some(questions),
            _ => None,
        }
    }

    /// The message of an `Other` prompt, when the payload maps to one.
    fn other_message_of(body: &Value) -> Option<String> {
        match pending_of(body) {
            PendingUpdate::Set(PendingInput::Other { message }) => Some(message),
            _ => None,
        }
    }

    /// The tool name and summary of a permission prompt, when the payload
    /// maps to one.
    fn permission_of(body: &Value) -> Option<(String, String)> {
        match pending_of(body) {
            PendingUpdate::Set(PendingInput::Permission { tool_name, summary }) => {
                Some((tool_name, summary))
            }
            _ => None,
        }
    }

    #[test]
    fn map_reports_the_flags_hook_status_used_to_read_from_event_names() {
        let mapped = |body: Value| map(&body).expect("the payload is mapped");
        let started = mapped(json!({"hook_event_name": "SessionStart", "source": "startup"}));
        assert!(started.agent_started, "a start starts the agent's run");
        assert!(!started.agent_ended);
        let logout = mapped(json!({"hook_event_name": "SessionEnd", "reason": "logout"}));
        assert!(logout.agent_ended, "a logout ends the agent");
        assert!(!logout.agent_started);
        let clear = mapped(json!({"hook_event_name": "SessionEnd", "reason": "clear"}));
        assert!(!clear.agent_ended, "a /clear ends only the conversation");
        let idle =
            mapped(json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"}));
        assert!(idle.weak_idle, "an idle prompt is a weak idle");
        let asking = mapped(
            json!({"hook_event_name": "Notification", "notification_type": "permission_prompt"}),
        );
        assert!(!asking.weak_idle, "a prompt notification is no idle at all");
        let stop = mapped(json!({
            "hook_event_name": "Stop",
            "background_tasks": [{"id": "b1", "type": "shell", "status": "running"}],
        }));
        assert_eq!(
            stop.background,
            Some(true),
            "the main agent's stop reports its work"
        );
        assert!(!stop.subagent);
        let prompt = mapped(json!({"hook_event_name": "UserPromptSubmit"}));
        assert_eq!(
            prompt.background, None,
            "only a stop reports background work"
        );
        let subagent = mapped(json!({"hook_event_name": "PostToolUse", "agent_id": "a1"}));
        assert!(subagent.subagent, "an agent id marks a subagent's event");
        let subagent_stop = mapped(json!({
            "hook_event_name": "Stop",
            "agent_id": "a1",
            "background_tasks": [{"id": "a1", "type": "subagent", "status": "running"}],
        }));
        assert_eq!(
            subagent_stop.background, None,
            "a subagent's stop says nothing about it"
        );
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
            "background_tasks": [{"id": "b1", "type": "shell", "status": "running"}],
        });
        assert_eq!(
            details(&body),
            HookDetails {
                event: "StopFailure".to_owned(),
                session_id: Some("0b6f4c7e-1111-4222-8333-944455556666".to_owned()),
                last_assistant_message: Some("Done.".to_owned()),
                permission_mode: Some("plan".to_owned()),
                failure_detail: Some("overloaded".to_owned()),
                reason: Some("overloaded".to_owned()),
                subagent: false,
                background_work: true,
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
                reason: None,
                subagent: false,
                background_work: false,
            }
        );
        // A wrong-typed field reads as absent, and `message` stands in for a
        // missing or non-string `reason`.
        let typed = json!({
            "session_id": 7,
            "reason": 3,
            "message": "boom",
            "background_tasks": {"status": "running"},
            "session_crons": "nope",
        });
        let read = details(&typed);
        assert_eq!(read.event, "");
        assert_eq!(read.session_id, None);
        assert_eq!(read.failure_detail.as_deref(), Some("boom"));
        assert!(!read.background_work, "non-arrays are no background work");
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

    #[test]
    fn a_long_question_and_its_labels_are_capped() {
        // A question, its header and its labels are all capped where they are
        // built, so no prompt can put an unbounded string on the wire.
        let long_input = json!({"questions": [{
            "question": "q".repeat(MESSAGE_MAX_CHARS + 50),
            "header": "h".repeat(500),
            "options": [{"label": "o".repeat(500)}, {"label": "short"}],
        }]});
        for event in ["PreToolUse", "PermissionRequest"] {
            let body = json!({
                "hook_event_name": event,
                "tool_name": "AskUserQuestion",
                "tool_input": long_input.clone(),
            });
            let questions = questions_of(&body).expect("the payload is a question prompt");
            assert_eq!(questions.len(), 1, "{event}");
            let asked = &questions[0];
            assert_eq!(
                asked.question,
                format!("{}…", "q".repeat(MESSAGE_MAX_CHARS - 1)),
                "{event}"
            );
            assert_eq!(asked.header.chars().count(), DETAIL_MAX_CHARS, "{event}");
            assert!(asked.header.ends_with('…'), "{event}");
            assert_eq!(asked.options.len(), 2, "{event}");
            assert_eq!(
                asked.options[0].chars().count(),
                DETAIL_MAX_CHARS,
                "{event}"
            );
            assert!(asked.options[0].ends_with('…'), "{event}");
            assert_eq!(asked.options[1], "short", "{event}");
        }
    }

    #[test]
    fn question_and_option_counts_are_capped() {
        // The first ones in order are kept; the rest are dropped.
        let items: Vec<Value> = (0..12)
            .map(|q| {
                let options: Vec<Value> = (0..12)
                    .map(|o| json!({ "label": format!("q{q}-o{o}") }))
                    .collect();
                json!({ "question": format!("question {q}"), "options": options })
            })
            .collect();
        let body = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "AskUserQuestion",
            "tool_input": {"questions": items},
        });
        let questions = questions_of(&body).expect("the payload is a question prompt");
        assert_eq!(questions.len(), MAX_QUESTIONS);
        assert_eq!(questions[0].question, "question 0");
        let last = MAX_QUESTIONS - 1;
        assert_eq!(questions[last].question, format!("question {last}"));
        for question in &questions {
            assert_eq!(question.options.len(), MAX_OPTIONS);
        }
        assert_eq!(questions[0].options[0], "q0-o0");
        assert_eq!(
            questions[0].options[MAX_OPTIONS - 1],
            format!("q0-o{}", MAX_OPTIONS - 1)
        );
    }

    #[test]
    fn a_long_elicitation_message_is_capped() {
        let body = json!({
            "hook_event_name": "Elicitation",
            "mcp_server_name": "linear",
            "message": "m".repeat(5000),
        });
        let message = other_message_of(&body).expect("the payload is an Other prompt");
        assert_eq!(message.chars().count(), MESSAGE_MAX_CHARS);
        assert!(message.starts_with("linear: mmm"), "{message}");
        assert!(message.ends_with('…'), "{message}");
    }

    #[test]
    fn a_long_notification_message_is_capped() {
        let body = json!({
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "n".repeat(5000),
        });
        assert_eq!(
            pending_of(&body),
            PendingUpdate::KeepOr(PendingInput::Other {
                message: format!("{}…", "n".repeat(MESSAGE_MAX_CHARS - 1)),
            })
        );
    }

    #[test]
    fn a_long_permission_tool_name_is_capped() {
        // The tool name rides every snapshot with the prompt, so it is capped
        // like the summary beside it.
        let body = json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "t".repeat(500),
            "tool_input": {"command": "ls"},
        });
        let (tool_name, summary) =
            permission_of(&body).expect("the payload is a permission prompt");
        assert_eq!(tool_name, format!("{}…", "t".repeat(DETAIL_MAX_CHARS - 1)));
        assert_eq!(summary, "ls");
    }

    #[test]
    fn json_prefix_matches_a_full_serialisation_cut_the_same_way() {
        let values = [
            json!({"url": "https://example.com"}),
            json!({"url": "y".repeat(300)}),
            json!({"k": "é".repeat(400)}),
            json!({"a": [1, 2, 3], "b": {"c": "z".repeat(10_000)}}),
        ];
        for value in values {
            assert_eq!(
                json_prefix(&value, DETAIL_MAX_CHARS),
                truncate(&value.to_string(), DETAIL_MAX_CHARS),
                "{value}"
            );
        }
        // A serialisation exactly at the cap is kept whole, with no ellipsis.
        let exact = json!({"k": "a".repeat(DETAIL_MAX_CHARS - 8)});
        assert_eq!(exact.to_string().chars().count(), DETAIL_MAX_CHARS);
        let prefix = json_prefix(&exact, DETAIL_MAX_CHARS);
        assert_eq!(prefix, exact.to_string());
        assert!(!prefix.ends_with('…'), "{prefix}");
    }
}
