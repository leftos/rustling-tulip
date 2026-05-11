//! Heuristic status detection for PTY-driven sessions.
//!
//! Watches the PTY output stream and infers `idle / working / awaiting_input`
//! by combining:
//! - regex matches against known Claude Code TUI prompt patterns
//! - an idle timeout: after N ms of silence following recent output, the
//!   session settles back to `idle` (still attached, just nothing to do)

use crate::session::{SessionRegistry, push_recent_action};
use protocol::{AttentionReason, SessionStatus};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tracing::warn;

const IDLE_AFTER: Duration = Duration::from_millis(800);
const SCROLLBACK_BYTES: usize = 8 * 1024;

pub struct AttentionEvent {
    pub session_id: String,
    pub reason: AttentionReason,
}

/// Spawn a background task that watches the given PTY broadcast for a session
/// and updates its status. `attention_tx` receives one event each time the
/// session transitions into `AwaitingInput` (used by the server to forward
/// `DaemonMessage::Attention` to clients).
pub fn watch(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    mut output: broadcast::Receiver<Vec<u8>>,
    attention_tx: mpsc::UnboundedSender<AttentionEvent>,
) {
    let registry = Arc::clone(registry);
    tokio::spawn(async move {
        let mut state = State::Idle;
        let mut last_output = Instant::now();
        let mut scrollback: Vec<u8> = Vec::with_capacity(SCROLLBACK_BYTES);

        loop {
            let next_idle_check = last_output + IDLE_AFTER;
            tokio::select! {
                msg = output.recv() => match msg {
                    Ok(bytes) => {
                        last_output = Instant::now();
                        append_scrollback(&mut scrollback, &bytes);
                        let new_state = classify(&scrollback, State::Working);
                        on_transition(
                            &registry,
                            &session_id,
                            &mut state,
                            new_state,
                            &attention_tx,
                        );
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(session_id = %session_id, lagged = n, "pty state lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                () = sleep_until(next_idle_check), if matches!(state, State::Working | State::AwaitingInput) => {
                    let new_state = classify(&scrollback, State::Idle);
                    on_transition(
                        &registry,
                        &session_id,
                        &mut state,
                        new_state,
                        &attention_tx,
                    );
                }
            }
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Working,
    AwaitingInput,
}

impl From<State> for SessionStatus {
    fn from(s: State) -> Self {
        match s {
            State::Idle => SessionStatus::Idle,
            State::Working => SessionStatus::Working,
            State::AwaitingInput => SessionStatus::AwaitingInput,
        }
    }
}

fn append_scrollback(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(bytes);
    if buf.len() > SCROLLBACK_BYTES {
        let drop = buf.len() - SCROLLBACK_BYTES;
        buf.drain(0..drop);
    }
}

fn classify(scrollback: &[u8], default: State) -> State {
    // Strip ANSI escape sequences and non-ASCII before matching: Claude's TUI
    // emits a lot of color codes that would otherwise break naive substring
    // matches. We work on the last ~2 KB of stripped output.
    let recent = strip_ansi(scrollback);
    let tail_start = recent.len().saturating_sub(2048);
    let tail = &recent[tail_start..];

    if matches_prompt(tail) {
        State::AwaitingInput
    } else {
        default
    }
}

fn matches_prompt(tail: &str) -> bool {
    // Permission-prompt heuristics. Order matters loosely; first match wins.
    //
    // 1. Numbered choices on consecutive lines: `1. <something>` `2. <something>`
    //    with a `❯` cursor or "(esc to cancel)" footer.
    // 2. "Do you want to" wording (Claude's edit/bash permission prompts).
    // 3. AskUserQuestion blocks contain "│" framing plus numbered options.
    let has_numbered = tail.contains("1.") && tail.contains("2.");
    let has_chevron = tail.contains("❯") || tail.contains(">>>");
    let has_do_you_want = tail.contains("Do you want to");
    let has_choose_option =
        tail.contains("Choose an option") || tail.contains("Press 1") || tail.contains("(y/n)");
    let has_ask_user = tail.contains("│") && tail.contains("AskUserQuestion");

    has_do_you_want || has_choose_option || has_ask_user || (has_numbered && has_chevron)
}

fn strip_ansi(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == 0x1B {
            // ESC
            if i + 1 < input.len() && input[i + 1] == b'[' {
                // CSI: ESC [ ... <terminator in 0x40..=0x7E>
                i += 2;
                while i < input.len() && !(0x40..=0x7E).contains(&input[i]) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            if i + 1 < input.len() && input[i + 1] == b']' {
                // OSC: ESC ] ... BEL or ESC \
                i += 2;
                while i < input.len() && input[i] != 0x07 {
                    if input[i] == 0x1B && i + 1 < input.len() && input[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                if i < input.len() && input[i] == 0x07 {
                    i += 1;
                }
                continue;
            }
            i += 1;
            continue;
        }
        if b == b'\r' {
            i += 1;
            continue;
        }
        if b.is_ascii() {
            out.push(b as char);
        }
        i += 1;
    }
    out
}

fn on_transition(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    current: &mut State,
    next: State,
    attention_tx: &mpsc::UnboundedSender<AttentionEvent>,
) {
    if next == *current {
        return;
    }
    let prev = *current;
    *current = next;
    registry.update(session_id, |rec| {
        rec.status = SessionStatus::from(next);
        if next == State::AwaitingInput && prev != State::AwaitingInput {
            push_recent_action(rec, "awaiting input".to_string());
        }
    });
    if next == State::AwaitingInput {
        let _ = attention_tx.send(AttentionEvent {
            session_id: session_id.to_string(),
            reason: AttentionReason::AwaitingInput,
        });
    }
}
