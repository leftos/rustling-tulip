//! Maps Claude Code hook payloads onto session status and pending input.
//!
//! The daemon's own status watcher (`pty_state`) guesses the status from the
//! byte stream; these hooks are the agent's own report. Reading is tolerant:
//! an unknown event, an unknown field and a body that is not a JSON object
//! are all ignored rather than refused.

use crate::server::Hub;
use protocol::{PendingInput, PendingQuestion, SessionStatus};
use serde_json::Value;

/// Characters of a plan kept before the text is cut short.
const PLAN_MAX_CHARS: usize = 2000;

/// Characters of a serialised tool input kept as a permission summary.
const DETAIL_MAX_CHARS: usize = 200;

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
pub fn apply_signal(hub: &Hub, session_id: &str, event_time: Option<u128>, signal: &HookSignal) {
    let mut entered_awaiting = false;
    hub.sessions.update(session_id, |record| {
        let Some(event_time) = event_time else {
            return;
        };
        if record.last_hook_time.is_some_and(|last| event_time <= last) {
            return;
        }
        record.last_hook_time = Some(event_time);
        record.hook_driven = true;
        if matches!(record.status, SessionStatus::Stopped | SessionStatus::Error) {
            return;
        }
        if let Some(status) = signal.status {
            entered_awaiting = crate::pty_state::apply_status(record, status);
        }
        match &signal.pending {
            PendingUpdate::Clear => record.pending_input = None,
            PendingUpdate::Set(pending) => record.pending_input = Some(pending.clone()),
            PendingUpdate::KeepOr(pending) => {
                if record.pending_input.is_none() {
                    record.pending_input = Some(pending.clone());
                }
            }
        }
    });
    if entered_awaiting {
        crate::pty_state::announce_awaiting_input(session_id, &hub.attention_tx);
    }
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
