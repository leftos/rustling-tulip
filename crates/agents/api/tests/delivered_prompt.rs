//! A Code Mode session's question must keep waiting while the agent's own
//! background work is delivered to it as a prompt.
//!
//! Claude Code 2.1.330 hands a command hook that delivery as a plain
//! `UserPromptSubmit` whose `prompt` is the delivered envelope (a
//! `<task-notification>` for a finished background task or Monitor event, an
//! `<event …>` for a Code Mode poll event) and which carries no `source` field
//! at all: the bundle builds the hook input as `prompt, ...!1, session_title`,
//! and keeps the prompt's origin (`promptSentBy.source`) for its in-process
//! callback hooks only. Each such delivery moved a session waiting on an
//! `AskUserQuestion` to `Working` while the card stayed on screen, so Needs you
//! (phone Home, the desktop list, the badge) dropped a question nobody had
//! answered.
//!
//! The payloads below are shaped as 2.1.330 sends them; the envelopes are the
//! transcript's own `queued_command` and `poll_events` texts, with paths and
//! ids neutralized.

#![expect(clippy::expect_used, reason = "test fixtures")]

use agent_api::hook_vocabulary::map;
use serde_json::{Value, json};

/// A `UserPromptSubmit` for `prompt`, as Claude Code 2.1.330 sends it to a
/// command hook: no `source`, whoever or whatever submitted the prompt.
fn prompt_submit(prompt: &str) -> Value {
    json!({
        "session_id": "00000000-0000-0000-0000-000000000001",
        "transcript_path": "C:/work/example/transcript.jsonl",
        "cwd": "C:/work/example",
        "permission_mode": "default",
        "hook_event_name": "UserPromptSubmit",
        "prompt": prompt,
        "session_title": "Example session",
    })
}

/// A queued command Claude Code delivers, the only delivery in its
/// submission, which ended a question's wait 9 s after its `PreToolUse`.
const TASK_NOTIFICATION: &str = "<task-notification>\n\
<task-id>task-1</task-id>\n\
<summary>Monitor event: \"example watch\"</summary>\n\
<event>PEER C:\\work\\example\\.tmp\\dispatch\\run-1</event>\n\
If this event is something the user would act on now, send a PushNotification. \
Routine or benign output doesn't need one.\n\
</task-notification>";

/// A Code Mode poll event, delivered while the eval that asked still runs.
const POLL_EVENT: &str = "<event kind=\"repl-status\" at=\"2026-01-01T00:00:00.000Z\" pending=\"1\">\
no output yet · 1 eval pending: #7 running for 1s, and its AskUserQuestion call \
hasn't started after 1s</event>";

#[test]
fn a_delivered_task_notification_without_a_source_spares_the_waiting_prompt() {
    let report = map(&prompt_submit(TASK_NOTIFICATION)).expect("a UserPromptSubmit maps");
    assert!(
        report.delivered,
        "a delivered <task-notification> reads as the owner's own prompt, \
         so it ends the question nobody answered: {report:?}"
    );
    assert!(report.spares_pending(), "the waiting prompt is not spared");
}

#[test]
fn a_delivered_poll_event_without_a_source_spares_the_waiting_prompt() {
    let report = map(&prompt_submit(POLL_EVENT)).expect("a UserPromptSubmit maps");
    assert!(
        report.delivered,
        "a delivered poll event with no `source` (2.1.330) reads as the \
         owner's own prompt: {report:?}"
    );
    assert!(report.spares_pending(), "the waiting prompt is not spared");
}

#[test]
fn a_delivered_notice_after_leading_whitespace_spares_the_waiting_prompt() {
    let prompt = format!("  \n\t{TASK_NOTIFICATION}");
    let report = map(&prompt_submit(&prompt)).expect("a UserPromptSubmit maps");
    assert!(
        report.delivered,
        "a delivered notice after leading whitespace reads as the owner's \
         own prompt: {report:?}"
    );
    assert!(report.spares_pending(), "the waiting prompt is not spared");
}

/// A poll event whose kind carries no attributes, which the CLI's own
/// validator accepts as `<event>` or `<event/>`.
#[test]
fn a_delivered_bare_event_element_spares_the_waiting_prompt() {
    for prompt in ["<event>deploy finished</event>", "<event/>", " \n<event>"] {
        let report = map(&prompt_submit(prompt)).expect("a UserPromptSubmit maps");
        assert!(
            report.delivered,
            "a delivered bare event element {prompt:?} reads as the owner's own prompt: {report:?}"
        );
        assert!(report.spares_pending(), "the waiting prompt is not spared");
    }
}

/// A poll event the host declared with fields: the hook sees
/// `[kind, at, fields].join("\n")`, with no element around it.
#[test]
fn a_field_declared_poll_event_spares_the_waiting_prompt() {
    for prompt in [
        "deploy-finished\n2026-11-21T18:00:00.000Z\nstatus: ok",
        "  repl-status\n2026-01-01T00:00:00Z",
    ] {
        let report = map(&prompt_submit(prompt)).expect("a UserPromptSubmit maps");
        assert!(
            report.delivered,
            "a field-declared poll event {prompt:?} reads as the owner's own prompt: {report:?}"
        );
        assert!(report.spares_pending(), "the waiting prompt is not spared");
    }
}

/// A typed prompt that opens with a kebab-case word is the owner's own:
/// its second line is no timestamp.
#[test]
fn a_typed_kebab_word_line_is_not_delivered() {
    for typed in ["fix-it\nplease look again", "fix-it", "deploy\n"] {
        let report = map(&prompt_submit(typed)).expect("a UserPromptSubmit maps");
        assert!(
            !report.delivered,
            "the owner's prompt {typed:?} reads as delivered"
        );
    }
}

/// A typed prompt that only starts like an event element is the owner's own.
#[test]
fn a_typed_eventual_prompt_is_not_delivered() {
    for typed in ["<eventual thing>", "<events>", "<event"] {
        let report = map(&prompt_submit(typed)).expect("a UserPromptSubmit maps");
        assert!(
            !report.delivered,
            "the owner's prompt {typed:?} reads as delivered"
        );
    }
}

/// The other side of the rule, which a fix must keep: what the owner types
/// still ends a wait (`a_typed_prompt_still_ends_a_waiting_question` in the
/// daemon's `hook_status`).
#[test]
fn a_typed_prompt_is_not_delivered() {
    for typed in [
        "go on",
        "  keep newest",
        "what does <task-notification> mean here?",
    ] {
        let report = map(&prompt_submit(typed)).expect("a UserPromptSubmit maps");
        assert!(
            !report.delivered,
            "the owner's prompt {typed:?} reads as delivered"
        );
    }
}
