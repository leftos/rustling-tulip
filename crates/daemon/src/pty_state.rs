//! Heuristic status detection for PTY-driven sessions.
//!
//! Watches the PTY output stream and infers `idle / working / awaiting_input`
//! by combining:
//! - regex matches against known Claude Code TUI prompt patterns
//! - an output-vs-input byte-volume comparison: genuine model/command output
//!   outpaces keystroke echo and promotes to `working`
//! - a terminal-title heartbeat: an agent that keeps repainting its title
//!   (spinner glyph, elapsed-time counter, progress text) is busy even when its
//!   byte volume dips below the echo budget, so a title change counts as
//!   activity (see [`crate::osc_title::TitleActivity`])
//! - an idle timeout with hysteresis: promotion to `working` happens on the
//!   first qualifying chunk, but demotion back to `idle` waits for
//!   [`IDLE_AFTER`] of silence, so a TUI element that repaints once a second
//!   (elapsed-time counter, slow spinner) holds `working` instead of flapping

use crate::osc_title::TitleActivity;
use crate::session::{SessionRegistry, push_recent_action};
use protocol::{AttentionReason, SessionStatus};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tracing::{debug, warn};

/// Silence required before a `Working` / `AwaitingInput` session settles back
/// to `Idle`. Deliberately asymmetric with promotion (which is immediate): the
/// slowest thing Claude's TUI repaints while busy is the elapsed-time counter
/// at 1 Hz, and an 800 ms timeout sat inside that gap, so the dot flickered
/// blue / grey once a second. 2.5 s clears a 1 Hz cadence with room for
/// scheduler jitter while still reading as "idle" promptly once the model
/// really stops.
const IDLE_AFTER: Duration = Duration::from_millis(2500);
const SCROLLBACK_BYTES: usize = 8 * 1024;

/// Sliding window used to compare input vs output byte volume. Chosen to be
/// long enough to smooth out inter-keystroke timing for slow typists, short
/// enough that the dot reacts to model output within a fraction of a second.
const VOLUME_WINDOW: Duration = Duration::from_millis(750);

/// Output bytes per input byte that we still consider "echo / TUI redraw"
/// rather than model output. One keystroke in codex/Claude's input box can
/// emit cursor moves, color resets, and a full line redraw — easily a few
/// hundred bytes. Picked generously so a fast typist never trips Working;
/// the model's streaming output dwarfs this budget within one window.
const ECHO_BUDGET_PER_INPUT_BYTE: u64 = 1024;

/// Minimum excess (output above echo budget) before we flip to Working.
/// Stops a stray cursor-position report or title escape from blipping the
/// dot blue between keystrokes.
const MIN_WORKING_EXCESS: u64 = 192;

/// Shells echo commands and prompts, but a real command's output should
/// outpace that echo quickly. PowerShell's `PSReadLine` drives this — one
/// keystroke can repaint the current line with syntax-highlighted colors,
/// emitting cursor-position escapes, color resets, and the redrawn line
/// (easily 60–200 bytes per char). Keep this looser than the agent TUI
/// budget for `ls`-class commands but generous enough that typing doesn't
/// flap the status dot.
const SHELL_ECHO_BUDGET_PER_INPUT_BYTE: u64 = 64;
const SHELL_MIN_WORKING_OUTPUT: u64 = 320;
const SHELL_MIN_WORKING_EXCESS: u64 = 256;

pub struct AttentionEvent {
    pub session_id: String,
    pub reason: AttentionReason,
}

/// Spawn a background task that watches the given PTY broadcast for a session
/// and updates its status.
///
/// `attention_tx` receives one event each time the session transitions into
/// `AwaitingInput` (used by the server to forward `DaemonMessage::Attention`
/// to clients).
///
/// `input_pulses` carries the byte count of each user input chunk forwarded
/// to the PTY. The watcher maintains a sliding window of recent input and
/// output volume; output that fits inside an "echo budget" proportional to
/// input volume keeps the session in its current state (typing into codex
/// echoes back via the PTY but should not flip the dot to Working). Output
/// that exceeds the budget — i.e. genuine model streaming — promotes to
/// Working as before.
///
/// A terminal-title change in the output stream also counts as activity, so an
/// agent whose spinner/elapsed-time title keeps repainting stays Working even
/// when the redraw is too small to clear the echo budget.
pub fn watch(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    mut output: broadcast::Receiver<Vec<u8>>,
    mut input_pulses: mpsc::UnboundedReceiver<usize>,
    attention_tx: mpsc::UnboundedSender<AttentionEvent>,
) {
    let registry = Arc::clone(registry);
    tokio::spawn(async move {
        let mut state = State::Idle;
        let mut last_output = Instant::now();
        let mut scrollback: Vec<u8> = Vec::with_capacity(SCROLLBACK_BYTES);
        let mut in_window: VecDeque<(Instant, u32)> = VecDeque::new();
        let mut out_window: VecDeque<(Instant, u32)> = VecDeque::new();
        let mut title_activity = TitleActivity::new();

        loop {
            let next_idle_check = last_output + IDLE_AFTER;
            tokio::select! {
                msg = output.recv() => match msg {
                    Ok(bytes) => {
                        let now = Instant::now();
                        last_output = now;
                        // Drain any input pulses that landed in our channel before
                        // (or alongside) this output chunk so the echo budget is
                        // computed against the right input volume. Without this,
                        // tokio::select polling can resolve the output arm before
                        // the input arm for a near-simultaneous keystroke, leaving
                        // in_sum at zero when we classify a TUI redraw caused by
                        // that keystroke.
                        while let Ok(n) = input_pulses.try_recv() {
                            push_window(&mut in_window, now, n);
                        }
                        append_scrollback(&mut scrollback, &bytes);
                        push_window(&mut out_window, now, bytes.len());
                        trim_window(&mut in_window, now);
                        trim_window(&mut out_window, now);

                        let in_sum = window_sum(&in_window);
                        let out_sum = window_sum(&out_window);
                        let budget = in_sum.saturating_mul(ECHO_BUDGET_PER_INPUT_BYTE);
                        let exceeds_echo = out_sum > budget.saturating_add(MIN_WORKING_EXCESS);
                        // A repainted terminal title (spinner / elapsed time) is
                        // activity in its own right, even when the redraw is too
                        // small to clear the echo budget.
                        let title_changed = title_activity.feed(&bytes);
                        let active = exceeds_echo || title_changed;

                        debug!(
                            session_id = %session_id,
                            in_sum,
                            out_sum,
                            budget,
                            exceeds_echo,
                            title_changed,
                            current = ?state,
                            "pty_state volume tick"
                        );

                        let new_state = classify(&scrollback, state, active);
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
                pulse = input_pulses.recv() => {
                    // `None` means the sender was dropped because the session
                    // is going away; the output stream's `Closed` arm will end
                    // this task shortly, so we just keep looping in the meantime.
                    if let Some(n) = pulse {
                        let now = Instant::now();
                        push_window(&mut in_window, now, n);
                        trim_window(&mut in_window, now);
                        // Input alone never flips state. The next output chunk
                        // will reclassify with this volume in scope.
                    }
                },
                () = sleep_until(next_idle_check), if matches!(state, State::Working | State::AwaitingInput) => {
                    let new_state = classify(&scrollback, State::Idle, false);
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

/// Spawn a background task that infers `idle / working` for a plain shell.
///
/// Plain shells deliberately avoid the Claude prompt classifier, but they can
/// still expose useful activity: command output should set the status to
/// `Working`, while echoed input and prompt redraws should leave it `Idle`.
pub fn watch_plain_shell(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    mut output: broadcast::Receiver<Vec<u8>>,
    mut input_pulses: mpsc::UnboundedReceiver<usize>,
) {
    let registry = Arc::clone(registry);
    tokio::spawn(async move {
        let mut state = State::Idle;
        let mut last_output = Instant::now();
        let mut in_window: VecDeque<(Instant, u32)> = VecDeque::new();
        let mut out_window: VecDeque<(Instant, u32)> = VecDeque::new();
        let mut title_activity = TitleActivity::new();

        loop {
            let next_idle_check = last_output + IDLE_AFTER;
            tokio::select! {
                msg = output.recv() => match msg {
                    Ok(bytes) => {
                        let now = Instant::now();
                        last_output = now;
                        // See watcher in `watch` for the rationale — drain
                        // pending input pulses before classifying so a fast
                        // typist's keystroke doesn't race ahead of its own
                        // echo into the Working state.
                        while let Ok(n) = input_pulses.try_recv() {
                            push_window(&mut in_window, now, n);
                        }
                        push_window(&mut out_window, now, bytes.len());
                        trim_window(&mut in_window, now);
                        trim_window(&mut out_window, now);

                        let in_sum = window_sum(&in_window);
                        let out_sum = window_sum(&out_window);
                        // A title change (e.g. a `cd` that repaints the cwd in
                        // the prompt) counts as activity alongside command output.
                        let title_changed = title_activity.feed(&bytes);
                        let next = if shell_output_is_working(in_sum, out_sum) || title_changed {
                            State::Working
                        } else {
                            state
                        };
                        on_plain_shell_transition(&registry, &session_id, &mut state, next);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(session_id = %session_id, lagged = n, "plain shell state lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                pulse = input_pulses.recv() => {
                    if let Some(n) = pulse {
                        let now = Instant::now();
                        push_window(&mut in_window, now, n);
                        trim_window(&mut in_window, now);
                    }
                },
                () = sleep_until(next_idle_check), if state == State::Working => {
                    on_plain_shell_transition(&registry, &session_id, &mut state, State::Idle);
                }
            }
        }
    });
}

fn push_window(window: &mut VecDeque<(Instant, u32)>, now: Instant, bytes: usize) {
    let n = u32::try_from(bytes).unwrap_or(u32::MAX);
    window.push_back((now, n));
}

fn trim_window(window: &mut VecDeque<(Instant, u32)>, now: Instant) {
    let cutoff = now.checked_sub(VOLUME_WINDOW).unwrap_or(now);
    while let Some(&(t, _)) = window.front() {
        if t < cutoff {
            window.pop_front();
        } else {
            break;
        }
    }
}

fn window_sum(window: &VecDeque<(Instant, u32)>) -> u64 {
    window.iter().map(|(_, n)| u64::from(*n)).sum()
}

fn shell_output_is_working(input_bytes: u64, output_bytes: u64) -> bool {
    let budget = input_bytes
        .saturating_mul(SHELL_ECHO_BUDGET_PER_INPUT_BYTE)
        .saturating_add(SHELL_MIN_WORKING_EXCESS);
    output_bytes >= SHELL_MIN_WORKING_OUTPUT && output_bytes > budget
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

fn classify(scrollback: &[u8], current: State, active: bool) -> State {
    // Strip ANSI escape sequences and non-ASCII before matching: Claude's TUI
    // emits a lot of color codes that would otherwise break naive substring
    // matches. We work on the last ~2 KB of stripped output.
    let recent = strip_ansi(scrollback);
    let tail_start = recent.len().saturating_sub(2048);
    let tail = &recent[tail_start..];

    if matches_prompt(tail) {
        State::AwaitingInput
    } else if active {
        // Genuine output beyond the echo budget, or a terminal-title repaint —
        // either way the session is doing something.
        State::Working
    } else {
        // No activity this tick — keep current state. When current is already
        // Working from a prior tick the IDLE_AFTER timer demotes us to Idle
        // once the PTY actually goes quiet.
        current
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

fn on_plain_shell_transition(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    current: &mut State,
    next: State,
) {
    if next == *current {
        return;
    }
    *current = next;
    registry.update(session_id, |rec| {
        rec.status = SessionStatus::from(next);
    });
}

#[cfg(test)]
mod tests {
    use super::{State, classify, shell_output_is_working};

    #[test]
    fn active_signal_promotes_idle_to_working() {
        // A title heartbeat with no qualifying byte volume still flips Idle to
        // Working via the `active` argument.
        assert_eq!(classify(b"", State::Idle, true), State::Working);
    }

    #[test]
    fn no_activity_keeps_current_state() {
        assert_eq!(classify(b"", State::Idle, false), State::Idle);
        assert_eq!(classify(b"", State::Working, false), State::Working);
    }

    #[test]
    fn prompt_match_beats_activity() {
        // Even while the title is repainting (active == true), a detected
        // permission prompt must win so the dot reflects AwaitingInput.
        assert_eq!(
            classify(b"Do you want to proceed?", State::Working, true),
            State::AwaitingInput
        );
    }

    #[test]
    fn shell_output_below_minimum_does_not_mark_working() {
        assert!(!shell_output_is_working(0, 120));
    }

    #[test]
    fn shell_echo_budget_suppresses_typed_input_redraws() {
        assert!(!shell_output_is_working(32, 220));
    }

    #[test]
    fn pwsh_syntax_highlight_burst_does_not_mark_working() {
        // Typing 4 chars into PSReadLine: ~100 bytes of redraw per char,
        // including cursor moves, color sets, and the re-emitted line tail.
        assert!(!shell_output_is_working(4, 400));
    }

    #[test]
    fn shell_command_output_marks_working() {
        assert!(shell_output_is_working(8, 1_200));
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod hysteresis_tests {
    use super::{IDLE_AFTER, watch, watch_plain_shell};
    use crate::paths::Dirs;
    use crate::session::{SessionRecord, SessionRegistry};
    use chrono::Utc;
    use protocol::{
        Agent, AppearanceOverrides, SessionKind, SessionMetrics, SessionMode, SessionStatus,
    };
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc};

    /// Cadence of the slowest TUI element the watcher must ride through
    /// without demoting: an elapsed-time counter repainting once a second.
    const HEARTBEAT: Duration = Duration::from_secs(1);

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-pty-state-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            lan_config_file: root.join("lan.json"),
            lan_cert_file: root.join("lan-cert.pem"),
            lan_key_file: root.join("lan-key.pem"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
        }
    }

    fn test_record(id: &str) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            label: id.to_string(),
            default_label: id.to_string(),
            user_label: None,
            kind: SessionKind::Standalone,
            members: Vec::new(),
            mode: SessionMode::Interactive,
            started_at: Utc::now(),
            status: SessionStatus::Idle,
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: Vec::new(),
            pty: None,
            headless: None,
            workspace_id: None,
            agent: Agent::default(),
            terminal_title: None,
            program_name: None,
            current_cwd: None,
            appearance: AppearanceOverrides::default(),
            spawn_config: None,
            is_abandoned: false,
            is_inactive: false,
            worktree_paths: Vec::new(),
            last_prompt: None,
            input_notifier: None,
            scrollback_snapshot_req: None,
        }
    }

    struct Harness {
        registry: Arc<SessionRegistry>,
        output: broadcast::Sender<Vec<u8>>,
        _input: mpsc::UnboundedSender<usize>,
        _attention: mpsc::UnboundedReceiver<super::AttentionEvent>,
    }

    const SESSION: &str = "s1";

    fn start(tag: &str, plain_shell: bool) -> Harness {
        let registry = SessionRegistry::new(scratch_dirs(tag));
        registry.insert(test_record(SESSION));
        let (output, output_rx) = broadcast::channel(64);
        let (input, input_rx) = mpsc::unbounded_channel();
        let (attention_tx, attention_rx) = mpsc::unbounded_channel();
        if plain_shell {
            watch_plain_shell(&registry, SESSION.to_string(), output_rx, input_rx);
        } else {
            watch(
                &registry,
                SESSION.to_string(),
                output_rx,
                input_rx,
                attention_tx,
            );
        }
        Harness {
            registry,
            output,
            _input: input,
            _attention: attention_rx,
        }
    }

    impl Harness {
        fn status(&self) -> SessionStatus {
            let rec = self.registry.get(SESSION).expect("session registered");
            let guard = rec.lock().expect("record lock");
            guard.status
        }

        /// Emit one elapsed-time title repaint, e.g. `ESC ] 0 ; ⠋ 3s BEL`.
        fn heartbeat(&self, tick: u32) {
            let frame = format!("\x1b]0;\u{280b} {tick}s\x07");
            self.output
                .send(frame.into_bytes())
                .expect("watcher still subscribed");
        }
    }

    /// Let the spawned watcher task run until it has nothing left to poll,
    /// without advancing the paused clock.
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    async fn assert_heartbeat_holds_working(h: &Harness) {
        for tick in 0..5 {
            h.heartbeat(tick);
            settle().await;
            assert_eq!(h.status(), SessionStatus::Working, "tick {tick} promoted");
            tokio::time::sleep(HEARTBEAT).await;
            settle().await;
            assert_eq!(
                h.status(),
                SessionStatus::Working,
                "tick {tick}: demoted to idle between once-a-second repaints"
            );
        }
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(h.status(), SessionStatus::Idle, "silence past IDLE_AFTER");
    }

    #[tokio::test(start_paused = true)]
    async fn once_a_second_repaint_does_not_flicker_agent_session() {
        let h = start("agent", false);
        assert_heartbeat_holds_working(&h).await;
    }

    #[tokio::test(start_paused = true)]
    async fn once_a_second_repaint_does_not_flicker_plain_shell() {
        let h = start("shell", true);
        assert_heartbeat_holds_working(&h).await;
    }

    #[tokio::test(start_paused = true)]
    async fn promotion_is_immediate() {
        let h = start("promote", false);
        assert_eq!(h.status(), SessionStatus::Idle);
        h.heartbeat(0);
        settle().await;
        assert_eq!(h.status(), SessionStatus::Working);
    }
}
