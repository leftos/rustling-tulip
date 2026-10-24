//! Applies agent hook reports to session status and pending input.
//!
//! An agent's hooks arrive through the files and argv its spawn's hook plan
//! injects, whose shim posts to the daemon's hook endpoint, and the agent's
//! plugin maps each payload to a [`HookReport`]. The daemon's own status
//! watcher (`pty_state`) guesses the status from the byte stream; these
//! reports are the agent's own. This module orders them, holds the ones that
//! arrive before their session is inserted, and writes them to the record.

use crate::server::Hub;
use crate::server::options_from_request;
use crate::session::{PendingInsert, SessionRecord, push_recent_action};
use agent_api::{AgentPlugin, HookReport, PendingUpdate};
use protocol::{Agent, SessionStatus};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Bytes of a turn's last assistant message kept on the record.
const LAST_MESSAGE_MAX_BYTES: usize = 8 * 1024;

/// How long a hook event waits for the session it names to be inserted. Past
/// it the event is dropped: the session either spawned long ago or never will.
const PENDING_HOOK_TTL: Duration = Duration::from_secs(30);

/// Most events held for one session id while it is being spawned.
const PENDING_HOOK_MAX_PER_SESSION: usize = 32;

/// Most session ids whose early events are held at once.
const PENDING_HOOK_MAX_SESSIONS: usize = 64;

/// Most body bytes held for one session id; an event larger than this alone
/// is never held.
const PENDING_HOOK_MAX_BYTES_PER_SESSION: usize = 256 * 1024;

/// Most body bytes held across every session id.
const PENDING_HOOK_MAX_BYTES: usize = 8 * 1024 * 1024;

/// The plugin that maps an agent's hook events.
pub type PluginLookup = fn(Agent) -> &'static dyn AgentPlugin;

/// `text` cut to at most `max_bytes` UTF-8 bytes on a character boundary,
/// ending in `…`, which counts inside the cap. Text within the cap is
/// returned unchanged. A cap too small for the ellipsis keeps that many
/// leading bytes and appends nothing.
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
/// The report's other fields — a new conversation id, the turn's last
/// message, a failure line, background work — are written under the same
/// update as the status, after the ordering check and the terminal guard; a
/// permission mismatch it carries is logged.
pub fn apply_signal(hub: &Hub, session_id: &str, event_time: Option<u128>, report: &HookReport) {
    let mut applied = Applied::default();
    hub.sessions.update(session_id, |record| {
        applied = apply_to_record(record, event_time, report);
    });
    log_mismatch(session_id, applied.mismatch.as_ref());
    if applied.entered_awaiting {
        crate::pty_state::announce_awaiting_input(session_id, &hub.attention_tx);
    }
    if applied.agent_exited {
        hub.sessions.announce_agent_exited(session_id);
    }
}

/// What applying one hook event to a record did.
#[derive(Debug, Default)]
pub(crate) struct Applied {
    /// The record entered `AwaitingInput`.
    pub(crate) entered_awaiting: bool,
    /// The expected and reported permission modes, when a prompt's differ.
    pub(crate) mismatch: Option<(String, String)>,
    /// The agent inside the session's shell ended, and its end is to be
    /// announced (see [`SessionRecord::end_agent_in_shell`]).
    pub(crate) agent_exited: bool,
}

/// Apply one mapped event to `record`: the ordering check, the stamp and the
/// state writes, as [`apply_signal`] and the pending-event replay both need.
pub(crate) fn apply_to_record(
    record: &mut SessionRecord,
    event_time: Option<u128>,
    report: &HookReport,
) -> Applied {
    let Some(event_time) = event_time else {
        return Applied::default();
    };
    if record.last_hook_time.is_some_and(|last| event_time <= last) {
        return Applied::default();
    }
    record.last_hook_time = Some(event_time);
    record.hook_driven = true;
    if matches!(record.status, SessionStatus::Stopped | SessionStatus::Error) {
        return Applied::default();
    }
    // An agent typed into its shell starts a new run: the leaf names it again
    // and the run's end will be announced.
    if report.agent_started {
        record.start_agent_in_shell();
    }
    // A shell outlives the CLI whose hooks drive it: once that CLI ends, its
    // hooks stop arriving and the shell classifier owns the status again.
    // Clearing `hook_driven` flips the shell's watcher
    // (`pty_state::watch_plain_shell`) back to its classifier, re-seeded from
    // the record, whose `SHELL_IDLE_AFTER` silence ends a stale `Working`; an
    // `AwaitingInput` ends here, since the agent that asked has ended and no
    // other event would demote it.
    // An agent typed into its shell at spawn ends the same way, and its end
    // is announced once per run. A `/clear` ends only the conversation: the
    // agent runs on and starts the next one at once.
    let mut agent_exited = false;
    if report.agent_ended && record.shell_hosted() {
        record.hook_driven = false;
        if record.status == SessionStatus::AwaitingInput {
            crate::pty_state::apply_status(record, SessionStatus::Idle);
            record.pending_input = None;
        }
        agent_exited = record.end_agent_in_shell();
    }
    if leaves_pending_alone(record, report) {
        return Applied {
            entered_awaiting: false,
            mismatch: apply_details(record, report),
            agent_exited,
        };
    }
    // A subagent works on while the main agent is idle, so its events never
    // promote the session into `Working`.
    let entered_awaiting = report
        .status
        .filter(|status| !(report.subagent && *status == SessionStatus::Working))
        .is_some_and(|status| crate::pty_state::apply_status(record, status));
    match &report.pending {
        PendingUpdate::Clear => record.pending_input = None,
        PendingUpdate::Set(pending) => record.pending_input = Some(pending.clone()),
        PendingUpdate::KeepOr(pending) => {
            if record.pending_input.is_none() {
                record.pending_input = Some(pending.clone());
            }
        }
    }
    Applied {
        entered_awaiting,
        mismatch: apply_details(record, report),
        agent_exited,
    }
}

/// Whether an event must leave the record's status and pending input alone,
/// because the main agent's prompt is still waiting.
///
/// A subagent's event never ends or replaces a pending prompt: its tool calls
/// fire the same hooks while the main agent waits on the user, and its own
/// permission request counts only when no prompt is waiting (a pending value
/// left beside another status is stale and is replaced). A weak idle (an
/// idle the agent reports while it may still be waiting) is ignored while the
/// record waits on a pending prompt.
fn leaves_pending_alone(record: &SessionRecord, report: &HookReport) -> bool {
    if record.pending_input.is_none() {
        return false;
    }
    let waiting = record.status == SessionStatus::AwaitingInput;
    waiting && (report.subagent || report.weak_idle)
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

/// One hook event that arrived before its session was inserted: its raw
/// body, mapped by the session's plugin once the record exists.
#[derive(Debug, Clone)]
struct PendingHook {
    at: Instant,
    /// Arrival order across every session, which decides the oldest session
    /// to evict; two arrivals can share one `Instant`.
    seq: u64,
    event_time: Option<u128>,
    body: serde_json::Value,
    /// The body's length in bytes as it arrived.
    len: usize,
}

/// The held events by session id, and the arrival counter.
#[derive(Default)]
struct Held {
    sessions: HashMap<String, Vec<PendingHook>>,
    next_seq: u64,
}

/// Hook events that arrived for a session id the registry did not hold yet,
/// i.e. a spawn in flight, held as raw bodies: which plugin maps them is
/// known only once the record exists. Bounded by [`PENDING_HOOK_TTL`], the
/// count caps and the byte caps; an event is mapped and applied when the
/// spawn inserts its record, and dropped when it expires or a cap evicts it.
#[derive(Clone)]
pub struct PendingHooks {
    held: Arc<std::sync::Mutex<Held>>,
    plugins: PluginLookup,
}

impl Default for PendingHooks {
    fn default() -> Self {
        Self::with_plugins(agent_registry::plugin)
    }
}

impl PendingHooks {
    /// An empty buffer whose events are mapped by the plugin `plugins`
    /// answers for a record's hook agent.
    #[must_use]
    pub fn with_plugins(plugins: PluginLookup) -> Self {
        Self {
            held: Arc::default(),
            plugins,
        }
    }

    /// The report `record`'s hook plugin maps `body` to, with the options
    /// and trust its spawn recorded (none for a record without a spawn
    /// config). A plain shell maps with its hook agent, so one without a hook
    /// agent maps nothing.
    pub(crate) fn map_for_record(
        &self,
        record: &SessionRecord,
        body: &serde_json::Value,
    ) -> Option<HookReport> {
        let agent = record.hook_plugin_agent()?;
        let spawn = record.spawn_config.as_ref();
        let options =
            spawn.map(|spawn| options_from_request(&spawn.agent_options, spawn.model.as_deref()));
        let trusted = spawn.is_some_and(|spawn| spawn.dangerously_skip_permissions);
        (self.plugins)(agent).map_hook_event(body, options.as_ref(), trusted)
    }

    /// Hold the raw event `body`, `len` bytes long, for `session_id` until
    /// its spawn inserts the session.
    ///
    /// The buffer lock is also the insert's lock: an event either lands in
    /// the buffer before the insert takes it (and is mapped and replayed into
    /// the record), or this call finds the session registered and maps and
    /// applies the event directly. Neither order can drop it.
    pub fn defer(
        &self,
        hub: &Hub,
        session_id: &str,
        event_time: Option<u128>,
        body: serde_json::Value,
        len: usize,
    ) {
        let mut held = crate::sync::lock(&self.held);
        let now = Instant::now();
        prune_expired(&mut held.sessions, now);
        if let Some(record) = hub.sessions.get(session_id) {
            drop(held);
            let report = self.map_for_record(&crate::sync::lock(&record), &body);
            if let Some(report) = report {
                apply_signal(hub, session_id, event_time, &report);
            }
            return;
        }
        if len > PENDING_HOOK_MAX_BYTES_PER_SESSION {
            debug!(
                session_id,
                len, "hook event larger than the held-event cap dropped"
            );
            return;
        }
        let seq = held.next_seq;
        held.next_seq += 1;
        hold(
            &mut held.sessions,
            session_id,
            PendingHook {
                at: now,
                seq,
                event_time,
                body,
                len,
            },
        );
        debug!(session_id, "hook event held until its session is inserted");
    }

    /// Insert `record` with every held event for its id mapped by its hook
    /// plugin and applied, under the buffer lock so a concurrent
    /// [`Self::defer`] cannot land between the take and the insert. Answers
    /// whether the record still holds an `AwaitingInput` prompt after the
    /// whole replay, so the caller raises its one `Attention` after
    /// publishing the session: a later event in the buffer may have cleared
    /// the prompt an earlier one set.
    pub fn insert_with_pending<'a>(
        &self,
        hub: &'a Hub,
        mut record: SessionRecord,
    ) -> (PendingInsert<'a>, bool) {
        let session_id = record.id.clone();
        let mut held = crate::sync::lock(&self.held);
        prune_expired(&mut held.sessions, Instant::now());
        let events = held.sessions.remove(&session_id).unwrap_or_default();
        for event in events {
            let Some(report) = self.map_for_record(&record, &event.body) else {
                continue;
            };
            // An agent's end held before its session was published has no
            // client that saw it start, so it is not announced.
            let applied = apply_to_record(&mut record, event.event_time, &report);
            log_mismatch(&session_id, applied.mismatch.as_ref());
        }
        let awaiting =
            record.status == SessionStatus::AwaitingInput && record.pending_input.is_some();
        (hub.sessions.insert_pending(record), awaiting)
    }

    /// The hook times of the events held for `session_id`, oldest first.
    #[cfg(test)]
    pub(crate) fn held_times(&self, session_id: &str) -> Vec<Option<u128>> {
        crate::sync::lock(&self.held)
            .sessions
            .get(session_id)
            .map(|events| events.iter().map(|event| event.event_time).collect())
            .unwrap_or_default()
    }
}

/// Add `event` to `session_id`'s held events, first evicting what the caps
/// make room for: the oldest session at the session cap, the session's
/// oldest events at its count and byte caps, then the oldest other sessions
/// at the total byte cap. `event` itself fits the per-session byte cap.
fn hold(sessions: &mut HashMap<String, Vec<PendingHook>>, session_id: &str, event: PendingHook) {
    if !sessions.contains_key(session_id) && sessions.len() >= PENDING_HOOK_MAX_SESSIONS {
        evict_oldest_session(sessions, session_id);
    }
    if let Some(events) = sessions.get_mut(session_id) {
        while !events.is_empty()
            && (events.len() >= PENDING_HOOK_MAX_PER_SESSION
                || held_bytes(events) + event.len > PENDING_HOOK_MAX_BYTES_PER_SESSION)
        {
            events.remove(0);
        }
    }
    while sessions
        .values()
        .map(|events| held_bytes(events))
        .sum::<usize>()
        + event.len
        > PENDING_HOOK_MAX_BYTES
    {
        if !evict_oldest_session(sessions, session_id) {
            break;
        }
    }
    sessions
        .entry(session_id.to_owned())
        .or_default()
        .push(event);
}

/// The body bytes `events` hold.
fn held_bytes(events: &[PendingHook]) -> usize {
    events.iter().map(|event| event.len).sum()
}

/// Drop the session other than `keep` whose newest held event arrived first,
/// answering whether there was one.
fn evict_oldest_session(sessions: &mut HashMap<String, Vec<PendingHook>>, keep: &str) -> bool {
    let oldest = sessions
        .iter()
        .filter(|(id, _)| id.as_str() != keep)
        .min_by_key(|(_, events)| events.last().map(|event| event.seq))
        .map(|(id, _)| id.clone());
    let Some(oldest) = oldest else {
        return false;
    };
    sessions.remove(&oldest);
    debug!(session_id = %oldest, "held hook events evicted to make room");
    true
}

/// Drop held events older than [`PENDING_HOOK_TTL`], and the ids left with
/// none.
fn prune_expired(sessions: &mut HashMap<String, Vec<PendingHook>>, now: Instant) {
    sessions.retain(|session_id, events| {
        let before = events.len();
        events.retain(|event| now.saturating_duration_since(event.at) < PENDING_HOOK_TTL);
        let expired = before - events.len();
        if expired > 0 {
            debug!(
                session_id,
                expired, "held hook events expired before their session was inserted"
            );
        }
        !events.is_empty()
    });
}

/// Write the report's fields beyond its status and pending input, answering
/// the permission mismatch it carries.
pub(crate) fn apply_details(
    record: &mut SessionRecord,
    report: &HookReport,
) -> Option<(String, String)> {
    if let Some(id) = report.conversation_id.as_deref() {
        update_conversation_id(record, id);
    }
    if let Some(message) = report.last_message.as_deref() {
        store_last_message(record, message);
    }
    // Only the main agent's turn end says whether work runs on; the status is
    // already `Idle` here, in the same update.
    if let Some(background) = report.background {
        record.background = background;
    }
    if let Some(line) = &report.failure {
        push_recent_action(record, line.clone());
    }
    report.permission_mismatch.clone()
}

/// A report naming a conversation moves the record to its id. A resumed one
/// reports the id it already had, so the write is a no-op; a value that is
/// not a plain id is refused, since it later becomes `claude --resume <id>`
/// and a transcript path.
fn update_conversation_id(record: &mut SessionRecord, id: &str) {
    if agent_api::is_plain_id(id) && record.claude_session_id.as_deref() != Some(id) {
        record.claude_session_id = Some(id.to_owned());
    }
}

/// A turn's end keeps the message the turn ended with. An empty message
/// keeps the previous one; a message is capped so it cannot grow the record
/// without bound.
fn store_last_message(record: &mut SessionRecord, message: &str) {
    let message = message.trim();
    if !message.is_empty() {
        record.last_assistant_message = Some(truncate_bytes(message, LAST_MESSAGE_MAX_BYTES));
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use protocol::{Agent, PendingInput, SessionMode};
    use serde_json::{Value, json};

    /// The report Claude's plugin maps `body` to, for a session with no
    /// spawn options.
    fn report(body: &Value) -> HookReport {
        agent_registry::plugin(Agent::Claude)
            .map_hook_event(body, None, false)
            .expect("the payload maps to a report")
    }

    /// The main agent's open permission prompt, last stamped at 1.
    fn record_with_open_prompt() -> SessionRecord {
        let mut record = crate::history::test_support::record("s1", SessionMode::Interactive);
        record.status = SessionStatus::AwaitingInput;
        record.pending_input = Some(main_prompt());
        record.last_hook_time = Some(1);
        record.hook_driven = true;
        record
    }

    fn main_prompt() -> PendingInput {
        PendingInput::Permission {
            tool_name: "Bash".to_owned(),
            summary: "cargo build".to_owned(),
        }
    }

    /// Apply `body` to `record` at hook time `time`, as the hook route does.
    fn apply_body(record: &mut SessionRecord, time: u128, body: &Value) -> Applied {
        apply_to_record(record, Some(time), &report(body))
    }

    #[test]
    fn subagent_post_tool_use_keeps_the_main_prompt() {
        let mut record = record_with_open_prompt();
        let body = json!({
            "hook_event_name": "PostToolUse",
            "agent_id": "a0123456789abcdef",
            "agent_type": "Explore",
            "tool_name": "Read",
        });
        apply_body(&mut record, 2, &body);
        assert_eq!(record.status, SessionStatus::AwaitingInput);
        assert_eq!(record.pending_input, Some(main_prompt()));
    }

    #[test]
    fn subagent_permission_request_does_not_replace_the_main_prompt() {
        let mut record = record_with_open_prompt();
        let body = json!({
            "hook_event_name": "PermissionRequest",
            "agent_id": "a0123456789abcdef",
            "tool_name": "Write",
            "tool_input": {"file_path": "notes.txt"},
        });
        apply_body(&mut record, 2, &body);
        assert_eq!(record.status, SessionStatus::AwaitingInput);
        assert_eq!(record.pending_input, Some(main_prompt()));
    }

    #[test]
    fn subagent_permission_request_sets_a_prompt_over_a_stale_pending_value() {
        let mut record = record_with_open_prompt();
        record.status = SessionStatus::Working;
        let body = json!({
            "hook_event_name": "PermissionRequest",
            "agent_id": "a0123456789abcdef",
            "tool_name": "Write",
            "tool_input": {"file_path": "notes.txt"},
        });
        apply_body(&mut record, 2, &body);
        assert_eq!(record.status, SessionStatus::AwaitingInput);
        assert!(
            matches!(
                &record.pending_input,
                Some(PendingInput::Permission { tool_name, .. }) if tool_name == "Write"
            ),
            "the subagent's prompt replaces the stale value: {:?}",
            record.pending_input
        );
    }

    #[test]
    fn subagent_permission_request_sets_a_prompt_when_none_is_pending() {
        let mut record = crate::history::test_support::record("s1", SessionMode::Interactive);
        record.status = SessionStatus::Working;
        record.last_hook_time = Some(1);
        let body = json!({
            "hook_event_name": "PermissionRequest",
            "agent_id": "a0123456789abcdef",
            "tool_name": "Write",
            "tool_input": {"file_path": "notes.txt"},
        });
        apply_body(&mut record, 2, &body);
        assert_eq!(record.status, SessionStatus::AwaitingInput);
        assert!(
            matches!(
                &record.pending_input,
                Some(PendingInput::Permission { tool_name, .. }) if tool_name == "Write"
            ),
            "the subagent's prompt is pending: {:?}",
            record.pending_input
        );
    }

    #[test]
    fn main_agent_post_tool_use_still_clears_the_prompt() {
        // `agent_type` alone marks a main agent run with `--agent`, and an
        // empty `agent_id` names no subagent.
        let bodies = [
            json!({"hook_event_name": "PostToolUse", "tool_name": "Bash"}),
            json!({"hook_event_name": "PostToolUse", "agent_type": "reviewer"}),
            json!({"hook_event_name": "PostToolUse", "agent_id": ""}),
        ];
        for body in bodies {
            let mut record = record_with_open_prompt();
            apply_body(&mut record, 2, &body);
            assert_eq!(record.status, SessionStatus::Working, "{body}");
            assert_eq!(record.pending_input, None, "{body}");
        }
    }

    fn idle_prompt() -> Value {
        json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"})
    }

    #[test]
    fn idle_prompt_over_an_open_prompt_is_ignored() {
        let mut record = record_with_open_prompt();
        apply_body(&mut record, 2, &idle_prompt());
        assert_eq!(record.status, SessionStatus::AwaitingInput);
        assert_eq!(record.pending_input, Some(main_prompt()));
    }

    #[test]
    fn idle_prompt_without_a_prompt_still_idles() {
        let mut working = crate::history::test_support::record("s1", SessionMode::Interactive);
        working.status = SessionStatus::Working;
        apply_body(&mut working, 2, &idle_prompt());
        assert_eq!(working.status, SessionStatus::Idle);

        let mut awaiting = record_with_open_prompt();
        awaiting.pending_input = None;
        apply_body(&mut awaiting, 2, &idle_prompt());
        assert_eq!(
            awaiting.status,
            SessionStatus::Idle,
            "nothing pending: the idle notification ends the wait"
        );
    }

    /// A plain shell outlives the CLI whose hooks drive it, so its `SessionEnd`
    /// is the last word on the question that CLI asked: the agent is gone and
    /// no other event would end the wait.
    #[test]
    fn a_plain_shell_session_end_ends_a_hook_prompt() {
        let mut record = crate::history::test_support::record("s1", SessionMode::PlainShell);
        record.status = SessionStatus::AwaitingInput;
        record.pending_input = Some(main_prompt());
        record.last_hook_time = Some(1);
        record.hook_driven = true;
        apply_body(
            &mut record,
            2,
            &json!({"hook_event_name": "SessionEnd", "reason": "prompt_input_exit"}),
        );
        assert_eq!(
            record.status,
            SessionStatus::Idle,
            "the ended agent's question is over"
        );
        assert_eq!(record.pending_input, None, "the prompt left with the agent");
        assert!(
            !record.hook_driven,
            "the shell classifier owns the status again"
        );
    }

    /// An interactive Claude session whose PTY child is the shell, hook-driven
    /// mid-turn, its leaf naming the agent.
    fn agent_in_shell_record() -> SessionRecord {
        let mut record = crate::history::test_support::record("s1", SessionMode::Interactive);
        record.agent_in_shell = true;
        record.shell_label = Some("pwsh".to_owned());
        record.program_name = Some("claude".to_owned());
        record.status = SessionStatus::Working;
        record.last_hook_time = Some(1);
        record.hook_driven = true;
        record
    }

    fn session_end() -> Value {
        json!({"hook_event_name": "SessionEnd", "reason": "prompt_input_exit"})
    }

    fn session_start() -> Value {
        json!({"hook_event_name": "SessionStart", "source": "startup"})
    }

    #[test]
    fn session_end_on_an_agent_in_shell_record_leaves_hook_driven_and_announces_once() {
        let mut record = agent_in_shell_record();
        record.status = SessionStatus::AwaitingInput;
        record.pending_input = Some(main_prompt());
        assert!(
            apply_body(&mut record, 2, &session_end()).agent_exited,
            "the agent's end is announced"
        );
        assert!(!record.hook_driven, "the shell classifier owns the status");
        assert_eq!(record.status, SessionStatus::Idle);
        assert_eq!(record.pending_input, None, "the prompt left with the agent");
        assert!(
            !apply_body(&mut record, 3, &session_end()).agent_exited,
            "a second end of the same run announces nothing"
        );

        let mut shell = crate::history::test_support::record("s2", SessionMode::PlainShell);
        shell.hook_driven = true;
        assert!(
            !apply_body(&mut shell, 2, &session_end()).agent_exited,
            "a plain shell's agent was typed by its user"
        );
        let mut direct = record_with_open_prompt();
        assert!(!apply_body(&mut direct, 2, &session_end()).agent_exited);
        assert!(
            direct.hook_driven,
            "an agent that is the PTY child keeps its hooks"
        );
    }

    /// `/clear` ends one conversation and starts the next in the same run:
    /// the agent is still running.
    #[test]
    fn a_clear_session_end_is_not_an_agent_exit() {
        let mut record = agent_in_shell_record();
        let clear = json!({"hook_event_name": "SessionEnd", "reason": "clear"});
        assert!(
            !apply_body(&mut record, 2, &clear).agent_exited,
            "a cleared conversation announces no exit"
        );
        assert!(record.hook_driven, "the agent's hooks still drive it");
        assert_eq!(record.program_name.as_deref(), Some("claude"));
        apply_body(
            &mut record,
            3,
            &json!({"hook_event_name": "SessionStart", "source": "clear"}),
        );
        assert!(
            apply_body(&mut record, 4, &session_end()).agent_exited,
            "the run's real end is announced"
        );
    }

    #[test]
    fn a_new_session_start_rearms_the_announcement() {
        let mut record = agent_in_shell_record();
        assert!(apply_body(&mut record, 2, &session_end()).agent_exited);
        assert!(!apply_body(&mut record, 3, &session_start()).agent_exited);
        assert!(record.hook_driven, "the new run's hooks drive the session");
        assert!(
            apply_body(&mut record, 4, &session_end()).agent_exited,
            "the next run's end is announced again"
        );
    }

    #[test]
    fn program_name_follows_the_child() {
        let mut record = agent_in_shell_record();
        apply_body(&mut record, 2, &session_end());
        assert_eq!(
            record.program_name.as_deref(),
            Some("pwsh"),
            "the shell is what runs once the agent ended"
        );
        apply_body(&mut record, 3, &session_start());
        assert_eq!(
            record.program_name.as_deref(),
            Some("claude"),
            "the agent runs again"
        );
    }

    /// A hook-driven record mid-turn, last stamped at 1.
    fn turn_record() -> SessionRecord {
        let mut record = crate::history::test_support::record("s1", SessionMode::Interactive);
        record.status = SessionStatus::Working;
        record.last_hook_time = Some(1);
        record.hook_driven = true;
        record
    }

    /// A main-agent `Stop` carrying `tasks` and `crons`.
    fn main_stop(tasks: &Value, crons: &Value) -> Value {
        json!({
            "hook_event_name": "Stop",
            "last_assistant_message": "started",
            "background_tasks": tasks,
            "session_crons": crons,
        })
    }

    #[test]
    fn a_main_stop_with_a_running_task_sets_background_beside_idle() {
        let mut record = turn_record();
        let tasks = json!([{"id": "b1", "type": "shell", "status": "running"}]);
        apply_body(&mut record, 2, &main_stop(&tasks, &json!([])));
        assert_eq!(record.status, SessionStatus::Idle);
        assert!(record.background, "a running shell is background work");
        assert!(record.snapshot().background, "the snapshot carries it");
    }

    #[test]
    fn a_main_stop_with_a_one_shot_cron_sets_background() {
        let mut record = turn_record();
        let crons = json!([{"id": "c1", "schedule": "21 12 * * *", "recurring": false}]);
        apply_body(&mut record, 2, &main_stop(&json!([]), &crons));
        assert_eq!(record.status, SessionStatus::Idle);
        assert!(record.background, "a pending wakeup is background work");
    }

    #[test]
    fn a_recurring_cron_alone_leaves_background_false() {
        let mut record = turn_record();
        let crons = json!([{"id": "c1", "schedule": "* * * * *", "recurring": true}]);
        apply_body(&mut record, 2, &main_stop(&json!([]), &crons));
        assert_eq!(record.status, SessionStatus::Idle);
        assert!(!record.background);
    }

    #[test]
    fn a_main_stop_with_no_running_work_leaves_background_false() {
        let bodies = [
            main_stop(&json!([]), &json!([])),
            json!({"hook_event_name": "Stop", "last_assistant_message": "done"}),
            main_stop(
                &json!([{"id": "b1", "type": "shell", "status": "completed"}]),
                &json!([]),
            ),
            main_stop(&json!([7, {"status": 3}]), &json!([{"recurring": "no"}])),
        ];
        for body in bodies {
            let mut record = turn_record();
            record.status = SessionStatus::Idle;
            record.background = true;
            apply_body(&mut record, 2, &body);
            assert_eq!(record.status, SessionStatus::Idle, "{body}");
            assert!(!record.background, "{body}");
        }
    }

    #[test]
    fn a_subagent_stop_leaves_background_alone() {
        let mut set = turn_record();
        set.status = SessionStatus::Idle;
        set.background = true;
        apply_body(
            &mut set,
            2,
            &json!({"hook_event_name": "Stop", "agent_id": "a1", "background_tasks": []}),
        );
        assert!(set.background, "a subagent's Stop does not clear it");

        let mut unset = turn_record();
        unset.status = SessionStatus::Idle;
        apply_body(
            &mut unset,
            2,
            &json!({
                "hook_event_name": "Stop",
                "agent_id": "a1",
                "background_tasks": [{"id": "a1", "type": "subagent", "status": "running"}],
            }),
        );
        assert!(!unset.background, "a subagent's Stop does not set it");
    }

    #[test]
    fn a_prompt_submit_after_a_background_stop_clears_it() {
        let mut record = turn_record();
        let tasks = json!([{"id": "b1", "type": "shell", "status": "running"}]);
        apply_body(&mut record, 2, &main_stop(&tasks, &json!([])));
        assert!(record.background);
        apply_body(
            &mut record,
            3,
            &json!({"hook_event_name": "UserPromptSubmit", "prompt": "<task-notification>"}),
        );
        assert_eq!(record.status, SessionStatus::Working);
        assert!(!record.background, "leaving Idle clears the flag");
    }

    #[test]
    fn a_subagent_tool_event_never_promotes_to_working() {
        for event in ["PreToolUse", "PostToolUse", "PostToolUseFailure"] {
            let mut record = turn_record();
            record.status = SessionStatus::Idle;
            record.background = true;
            apply_body(
                &mut record,
                2,
                &json!({
                    "hook_event_name": event,
                    "agent_id": "a1",
                    "agent_type": "general-purpose",
                    "tool_name": "Bash",
                }),
            );
            assert_eq!(record.status, SessionStatus::Idle, "{event}");
            assert!(record.background, "{event}");
            assert_eq!(record.last_hook_time, Some(2), "{event} still stamps");
        }
    }

    /// The one plugin the early-event tests map every agent's events with.
    static TEST_AGENT: std::sync::LazyLock<agent_api::test_agent::TestAgent> =
        std::sync::LazyLock::new(agent_api::test_agent::TestAgent::default);

    fn test_agent_plugin(_agent: Agent) -> &'static dyn agent_api::AgentPlugin {
        &*TEST_AGENT
    }

    /// A hub whose hook events are mapped by the test agent's plugin.
    fn test_agent_hub(tag: &str) -> (Hub, crate::server::test_support::ScratchDir) {
        let (mut hub, scratch) = crate::server::test_support::test_hub(tag);
        hub.pending_hooks = PendingHooks::with_plugins(test_agent_plugin);
        (hub, scratch)
    }

    /// A `SessionStart` only the test agent's plugin maps: Claude's knows no
    /// `test_status`.
    fn test_session_start(status: &str) -> Value {
        json!({"test_event": "SessionStart", "test_status": status})
    }

    /// Hold `body`, `len` bytes long, for `session_id` at hook time `time`,
    /// as the hook route does for a session not inserted yet.
    fn hold(hub: &Hub, session_id: &str, time: u128, body: Value, len: usize) {
        hub.pending_hooks
            .defer(hub, session_id, Some(time), body, len);
    }

    /// Insert `record` with its held events applied and answer what the
    /// inserted record holds: its status, last hook time and last message.
    fn insert(hub: &Hub, record: SessionRecord) -> (SessionStatus, Option<u128>, Option<String>) {
        let (pending, _awaiting) = hub.pending_hooks.insert_with_pending(hub, record);
        let seen = {
            let record = crate::sync::lock(pending.arc());
            (
                record.status,
                record.last_hook_time,
                record.last_assistant_message.clone(),
            )
        };
        pending.publish();
        seen
    }

    #[tokio::test]
    async fn an_early_session_start_is_held_then_mapped_by_the_records_plugin() {
        let (hub, _scratch) = test_agent_hub("early-test-agent");
        let body = test_session_start("working");
        assert!(
            agent_registry::plugin(Agent::Claude)
                .map_hook_event(&body, None, false)
                .is_none(),
            "only the test agent maps this body"
        );
        hold(&hub, "s1", 5, body, 64);
        assert!(hub.sessions.get("s1").is_none(), "not inserted yet");
        assert_eq!(hub.pending_hooks.held_times("s1"), [Some(5)]);
        let (status, time, message) = insert(
            &hub,
            crate::history::test_support::record("s1", SessionMode::Interactive),
        );
        assert_eq!(status, SessionStatus::Working);
        assert_eq!(time, Some(5));
        assert_eq!(
            message.as_deref(),
            Some("none trusted=false"),
            "the record's plugin mapped the held body"
        );
        assert_eq!(hub.pending_hooks.held_times("s1"), [], "the drain took it");
    }

    #[tokio::test]
    async fn an_early_event_for_a_plain_shell_maps_with_its_hook_agent() {
        let (hub, _scratch) = test_agent_hub("early-shell-hook-agent");
        hold(&hub, "s1", 5, test_session_start("working"), 64);
        let mut record = crate::history::test_support::record("s1", SessionMode::PlainShell);
        record.hook_agent = Some(Agent::Claude);
        let (status, time, message) = insert(&hub, record);
        assert_eq!(status, SessionStatus::Working);
        assert_eq!(time, Some(5));
        assert_eq!(message.as_deref(), Some("none trusted=false"));
    }

    #[tokio::test]
    async fn a_plain_shell_without_a_hook_agent_maps_nothing() {
        let (hub, _scratch) = test_agent_hub("early-shell-no-hook-agent");
        hold(&hub, "s1", 5, test_session_start("working"), 64);
        let (status, time, message) = insert(
            &hub,
            crate::history::test_support::record("s1", SessionMode::PlainShell),
        );
        assert_eq!((status, time, message), (SessionStatus::Idle, None, None));
        // An event that finds the shell inserted maps nothing either.
        hold(&hub, "s1", 6, test_session_start("working"), 64);
        let shell = hub.sessions.get("s1").expect("the shell is inserted");
        let shell = crate::sync::lock(&shell);
        assert_eq!(shell.status, SessionStatus::Idle);
        assert_eq!(shell.last_hook_time, None);
        assert!(!shell.hook_driven);
    }

    #[tokio::test]
    async fn held_events_are_capped_by_bytes_per_session() {
        let (hub, _scratch) = test_agent_hub("early-byte-cap");
        let third = PENDING_HOOK_MAX_BYTES_PER_SESSION / 3 + 1;
        for time in 1..=3 {
            hold(&hub, "s1", time, test_session_start("working"), third);
        }
        assert_eq!(
            hub.pending_hooks.held_times("s1"),
            [Some(2), Some(3)],
            "the oldest event made room for the third"
        );
        hold(
            &hub,
            "s2",
            1,
            test_session_start("working"),
            PENDING_HOOK_MAX_BYTES_PER_SESSION + 1,
        );
        assert_eq!(
            hub.pending_hooks.held_times("s2"),
            [],
            "an event over the whole cap is never held"
        );
        for time in 1..=33 {
            hold(&hub, "s3", time, test_session_start("working"), 1);
        }
        let held = hub.pending_hooks.held_times("s3");
        assert_eq!(held.len(), PENDING_HOOK_MAX_PER_SESSION);
        assert_eq!(held.first(), Some(&Some(2)), "the count cap still holds");
    }

    #[tokio::test]
    async fn held_events_evict_the_oldest_session_at_the_total_cap() {
        let (hub, _scratch) = test_agent_hub("early-total-cap");
        let per = PENDING_HOOK_MAX_BYTES_PER_SESSION;
        let full = PENDING_HOOK_MAX_BYTES / per;
        for i in 0..full {
            hold(
                &hub,
                &format!("s{i}"),
                1,
                test_session_start("working"),
                per,
            );
        }
        for i in 0..full {
            assert_eq!(hub.pending_hooks.held_times(&format!("s{i}")), [Some(1)]);
        }
        hold(&hub, "late", 1, test_session_start("working"), per);
        assert_eq!(
            hub.pending_hooks.held_times("s0"),
            [],
            "the oldest session made room"
        );
        assert_eq!(hub.pending_hooks.held_times("s1"), [Some(1)]);
        assert_eq!(hub.pending_hooks.held_times("late"), [Some(1)]);
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
}
