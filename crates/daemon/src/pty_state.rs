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
use crate::session::{SessionRecord, SessionRegistry, push_recent_action};
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

/// The PTY streams a status watcher reads: the session's output broadcast and
/// the byte counts of the user input forwarded to it.
pub struct WatchStreams {
    pub output: broadcast::Receiver<Vec<u8>>,
    pub input_pulses: mpsc::UnboundedReceiver<usize>,
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
///
/// `initial` is the status the session's record already carries: a reattached
/// session comes back `Idle` or `AwaitingInput`, since
/// `SessionRegistry::insert_reattached` restores those two and stamps any
/// other stored status `Idle` at the restart. The watcher has to start there:
/// seeded `Idle`, a record restored `AwaitingInput` would never arm the idle
/// timer that reclassifies it, so the restored status would stick until the
/// agent's next byte.
pub fn watch(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    streams: WatchStreams,
    attention_tx: mpsc::UnboundedSender<AttentionEvent>,
    initial: SessionStatus,
) {
    let registry = Arc::clone(registry);
    let WatchStreams {
        mut output,
        mut input_pulses,
    } = streams;
    tokio::spawn(async move {
        let mut state = State::seed(initial);
        let mut last_output = Instant::now();
        // The idle timer fires once per stretch of silence: a hook-reported
        // `AwaitingInput` whose prompt still matches holds without rearming,
        // and the next output starts a new stretch.
        let mut idle_checked = false;
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
                        idle_checked = false;
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

                        let read = record_status(&registry, &session_id);
                        if read.is_some_and(|read| read.hook_driven) {
                            // Hooks own every promotion while they are live.
                            // The windows and the title activity above keep
                            // feeding the two demotions the idle timer runs.
                            debug!(
                                session_id = %session_id,
                                "hook-driven: classifier output ignored"
                            );
                        } else if let Some(read) = read {
                            let new_state = classify(&scrollback, state, active);
                            on_transition(
                                &registry,
                                &session_id,
                                &mut state,
                                new_state,
                                read,
                                &attention_tx,
                            );
                        }
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
                () = sleep_until(next_idle_check),
                    if !idle_checked
                        && record_status(&registry, &session_id).is_some_and(|read| {
                            matches!(
                                read.status,
                                SessionStatus::Working | SessionStatus::AwaitingInput
                            )
                        }) => {
                    idle_checked = true;
                    on_idle(
                        &registry,
                        &session_id,
                        &scrollback,
                        &mut state,
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
/// `initial` is the status the session's record already carries. A plain
/// shell only moves between `Idle` and `Working`, and its idle timer only
/// fires from `Working`, so any other stored status seeds `Idle` (see
/// [`State::seed_plain_shell`]): seeded `AwaitingInput` the watcher would
/// never leave it.
pub fn watch_plain_shell(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    streams: WatchStreams,
    initial: SessionStatus,
) {
    let registry = Arc::clone(registry);
    let WatchStreams {
        mut output,
        mut input_pulses,
    } = streams;
    tokio::spawn(async move {
        let mut state = State::seed_plain_shell(initial);
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
                        match record_status(&registry, &session_id) {
                            Some(read) if read.hook_driven => {
                                // Hooks own the status while they are live: a
                                // shell classifier promotion would overwrite a
                                // hook-reported `AwaitingInput` with `Working`.
                                // Keep the local state in step with the record
                                // so the idle tick reads the same status.
                                state = State::seed_plain_shell(read.status);
                            }
                            Some(read) => {
                                let next =
                                    if shell_output_is_working(in_sum, out_sum) || title_changed {
                                        State::Working
                                    } else {
                                        state
                                    };
                                on_plain_shell_transition(
                                    &registry,
                                    &session_id,
                                    &mut state,
                                    next,
                                    read,
                                );
                            }
                            // The record is gone; there is nothing to write.
                            None => {}
                        }
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
                () = sleep_until(next_idle_check),
                    if plain_shell_idle_due(&registry, &session_id, state) => {
                    let read = record_status(&registry, &session_id);
                    if let Some(read) = read
                        && read.hook_driven
                    {
                        // The record's status is the truth while hooks are
                        // live: sync the local state, then end a hook-reported
                        // `Working` turn whose PTY has gone quiet. The demotion
                        // re-checks the status and hook stamp under the lock,
                        // so a promotion that landed since the read wins. A
                        // hook-reported `AwaitingInput` is left alone.
                        state = State::seed_plain_shell(read.status);
                        if read.status == SessionStatus::Working {
                            state = State::Idle;
                            demote_after_silence(
                                &registry,
                                &session_id,
                                SessionStatus::Working,
                                read.last_hook_time,
                            );
                        }
                    } else if let Some(read) = read
                        && !read.hook_driven
                    {
                        // Hooks that have ended leave the record `Working`;
                        // adopt it so the demotion writes even when no PTY
                        // chunk synced the local state since the hook event.
                        state = State::seed_plain_shell(read.status);
                        on_plain_shell_transition(
                            &registry,
                            &session_id,
                            &mut state,
                            State::Idle,
                            read,
                        );
                    }
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

impl State {
    /// The agent watcher's starting point for a record already carrying
    /// `status`. Only `AwaitingInput` and `Working` are states the watcher
    /// acts on: after [`IDLE_AFTER`] of silence it reclassifies both against
    /// the scrollback, so `AwaitingInput` holds while the prompt is on screen
    /// and anything else settles to `Idle`. Every other stored status starts
    /// at `Idle`.
    fn seed(status: SessionStatus) -> Self {
        match status {
            SessionStatus::AwaitingInput => Self::AwaitingInput,
            SessionStatus::Working => Self::Working,
            SessionStatus::Spawning
            | SessionStatus::Idle
            | SessionStatus::Stopped
            | SessionStatus::Error => Self::Idle,
        }
    }

    /// The plain-shell watcher's starting point for a record already carrying
    /// `status`. A shell only moves between `Idle` and `Working`, and only
    /// `Working` arms its idle timer, so every other stored status starts at
    /// `Idle`.
    fn seed_plain_shell(status: SessionStatus) -> Self {
        match status {
            SessionStatus::Working => Self::Working,
            SessionStatus::Spawning
            | SessionStatus::Idle
            | SessionStatus::AwaitingInput
            | SessionStatus::Stopped
            | SessionStatus::Error => Self::Idle,
        }
    }
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

/// Whether a prompt is on screen: the prompt regexes match the last ~2 KB of
/// stripped output. ANSI escape sequences are stripped first, since Claude's
/// TUI emits a lot of color codes that would otherwise break naive substring
/// matches; multi-byte characters are kept whole. The byte offset is widened
/// to a character boundary so a glyph straddling it cannot panic the slice.
fn prompt_on_screen(scrollback: &[u8]) -> bool {
    let recent = strip_ansi(scrollback);
    let mut tail_start = recent.len().saturating_sub(2048);
    while !recent.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    matches_prompt(&recent[tail_start..])
}

/// The record fields a classifier read before deciding a transition, and the
/// write re-checks under the record lock: a hook event or a hook-driven change
/// that landed in between wins over the classifier.
#[derive(Clone, Copy, Debug)]
struct StatusRead {
    status: SessionStatus,
    hook_driven: bool,
    last_hook_time: Option<u128>,
}

/// The record's current status, whether hooks drive its promotions, and the
/// hook ordering stamp, as one snapshot the caller carries into its write.
fn record_status(registry: &Arc<SessionRegistry>, session_id: &str) -> Option<StatusRead> {
    let arc = registry.get(session_id)?;
    let guard = crate::sync::lock(&arc);
    Some(StatusRead {
        status: guard.status,
        hook_driven: guard.hook_driven,
        last_hook_time: guard.last_hook_time,
    })
}

/// Whether a plain shell's idle tick is due: the classifier's own `Working`,
/// or a record still reporting `Working` when the PTY goes quiet — a
/// hook-driven screen, or one whose hooks have since ended. A hook-reported
/// `AwaitingInput` never ticks here; the prompt matcher that would end it
/// belongs to the agent watcher.
fn plain_shell_idle_due(registry: &Arc<SessionRegistry>, session_id: &str, state: State) -> bool {
    match record_status(registry, session_id) {
        Some(read) if read.hook_driven => read.status == SessionStatus::Working,
        Some(read) => state == State::Working || read.status == SessionStatus::Working,
        None => state == State::Working,
    }
}

fn classify(scrollback: &[u8], current: State, active: bool) -> State {
    if prompt_on_screen(scrollback) {
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

/// Whether Codex's selection cursor opens the first option and the second
/// option follows it: a `› 1.` line, then at most two blank lines, then a
/// line starting `2.`. Codex draws the same `› ` as its composer cursor, so
/// neither the glyph alone nor a typed `› 1.` with numbered output elsewhere
/// in the tail is a prompt.
fn has_selection_cursor(tail: &str) -> bool {
    let lines: Vec<&str> = tail.lines().collect();
    lines.iter().enumerate().any(|(at, line)| {
        let Some(rest) = line.trim_start().strip_prefix('›') else {
            return false;
        };
        if !rest.trim_start().starts_with("1.") {
            return false;
        }
        let mut next = at + 1;
        while next < lines.len() && next - at <= 2 && lines[next].trim().is_empty() {
            next += 1;
        }
        lines
            .get(next)
            .is_some_and(|option| option.trim_start().starts_with("2."))
    })
}

fn matches_prompt(tail: &str) -> bool {
    // Permission-prompt heuristics. Order matters loosely; first match wins.
    //
    // 1. Numbered choices on consecutive lines: `1. <something>` `2. <something>`
    //    with a `❯` cursor, a Codex `› 1.` selection line, or "(esc to cancel)"
    //    footer.
    // 2. "Do you want to" wording (Claude's edit/bash permission prompts).
    // 3. AskUserQuestion blocks contain "│" framing plus numbered options.
    let has_numbered = tail.contains("1.") && tail.contains("2.");
    let has_chevron = tail.contains("❯") || tail.contains(">>>") || has_selection_cursor(tail);
    let has_do_you_want = tail.contains("Do you want to");
    let has_choose_option =
        tail.contains("Choose an option") || tail.contains("Press 1") || tail.contains("(y/n)");
    let has_ask_user = tail.contains("│") && tail.contains("AskUserQuestion");

    has_do_you_want || has_choose_option || has_ask_user || (has_numbered && has_chevron)
}

fn strip_ansi(input: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
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
        out.push(b);
        i += 1;
    }
    // Keep multi-byte characters whole and decode once: the prompt markers
    // `❯` and `│` are UTF-8 beyond ASCII, and dropping those bytes would hide
    // a real question prompt from `matches_prompt`.
    String::from_utf8_lossy(&out).into_owned()
}

/// Run the idle timer's transition once a silence has lasted [`IDLE_AFTER`].
///
/// A hook-driven session is demoted without the classifier: `Working` to
/// `Idle` because silence during hook-reported work means the turn was
/// interrupted (an Esc interrupt fires no closing hook), and `AwaitingInput`
/// to `Idle` once its prompt has left the screen, because a prompt dismissed
/// from another client fires no hook either. An `AwaitingInput` whose prompt
/// still matches holds. A session without hooks keeps the classifier's own
/// reclassification.
fn on_idle(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    scrollback: &[u8],
    state: &mut State,
    attention_tx: &mpsc::UnboundedSender<AttentionEvent>,
) {
    match record_status(registry, session_id) {
        Some(read) if read.hook_driven => {
            let demote = match read.status {
                SessionStatus::Working => true,
                SessionStatus::AwaitingInput => !prompt_on_screen(scrollback),
                _ => false,
            };
            if demote {
                *state = State::Idle;
                demote_after_silence(registry, session_id, read.status, read.last_hook_time);
            }
        }
        Some(read) => {
            let new_state = classify(scrollback, State::Idle, false);
            on_transition(registry, session_id, state, new_state, read, attention_tx);
        }
        None => {}
    }
}

/// Demote `session_id` from `expected` to `Idle`, clearing the prompt it was
/// waiting on.
///
/// The update checks `expected` and the hook ordering stamp again under the
/// record lock: a promotion that landed since the caller read the record wins,
/// and a status that left and returned through a newer hook event (an ABA) is
/// not demoted at the old deadline.
fn demote_after_silence(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    expected: SessionStatus,
    epoch: Option<u128>,
) {
    registry.update(session_id, |rec| {
        if rec.status != expected || rec.last_hook_time != epoch {
            return;
        }
        apply_status(rec, SessionStatus::Idle);
        if expected == SessionStatus::AwaitingInput {
            rec.pending_input = None;
        }
    });
}

/// The status transition path the PTY watcher uses.
///
/// Other paths write a status directly on the record — `SendInput`'s
/// owner-key clear, stop, park and the exit watcher; this is the one that
/// carries the `"awaiting input"` action and the single [`AttentionEvent`] on
/// entry.
///
/// `read` is the snapshot the classifier decided from. The update re-checks
/// the hook ordering stamp and the hook-driven flag under the lock, so a hook
/// event that landed after the classifier's read wins and its status is not
/// overwritten by the classifier's write. An unchanged status is a no-op. On
/// entry to `AwaitingInput` the record gains an `"awaiting input"` action and
/// one [`AttentionEvent`], sent after the registry update returns so a slow
/// consumer never holds the record lock. The write goes through
/// [`SessionRegistry::update`], which stamps `status_since` and syncs the
/// sidecar.
fn transition(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    next: SessionStatus,
    read: StatusRead,
    attention_tx: &mpsc::UnboundedSender<AttentionEvent>,
) {
    // An unchanged status is not a transition: skip the update so nothing is
    // mirrored to the sidecar or broadcast.
    if read.status == next {
        return;
    }
    let mut entered_awaiting = false;
    registry.update(session_id, |rec| {
        if rec.last_hook_time != read.last_hook_time || rec.hook_driven != read.hook_driven {
            return;
        }
        entered_awaiting = apply_status(rec, next);
    });
    if entered_awaiting {
        announce_awaiting_input(session_id, attention_tx);
    }
}

/// Apply `next` to `record` when it is a real change.
///
/// Sets the status and records the `"awaiting input"` action on entry.
/// Answers whether the record entered `AwaitingInput`, so a caller holding a
/// registry update can raise the one `Attention` after that update returns.
pub fn apply_status(record: &mut SessionRecord, next: SessionStatus) -> bool {
    if record.status == next {
        return false;
    }
    record.status = next;
    if next == SessionStatus::AwaitingInput {
        push_recent_action(record, "awaiting input".to_string());
        return true;
    }
    false
}

/// Send the one `Attention` an entry into `AwaitingInput` raises. Call it
/// after the registry update has returned, so a slow consumer never holds the
/// record lock.
pub fn announce_awaiting_input(
    session_id: &str,
    attention_tx: &mpsc::UnboundedSender<AttentionEvent>,
) {
    let _ = attention_tx.send(AttentionEvent {
        session_id: session_id.to_string(),
        reason: AttentionReason::AwaitingInput,
    });
}

fn on_transition(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    current: &mut State,
    next: State,
    read: StatusRead,
    attention_tx: &mpsc::UnboundedSender<AttentionEvent>,
) {
    if next == *current {
        return;
    }
    *current = next;
    transition(
        registry,
        session_id,
        SessionStatus::from(next),
        read,
        attention_tx,
    );
}

fn on_plain_shell_transition(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    current: &mut State,
    next: State,
    read: StatusRead,
) {
    if next == *current {
        return;
    }
    *current = next;
    registry.update(session_id, |rec| {
        if rec.hook_driven != read.hook_driven || rec.last_hook_time != read.last_hook_time {
            return;
        }
        rec.status = SessionStatus::from(next);
    });
}

#[cfg(test)]
mod tests {
    use super::{State, classify, matches_prompt, shell_output_is_working};

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
    fn prompt_tail_starts_on_a_char_boundary() {
        // The 2 KB tail begins inside the three-byte `❯`. Slicing at the raw
        // byte offset would panic, and a panicking watcher task stops
        // reporting status for the rest of the session.
        let mut screen = vec![b'a'; 2047];
        screen.extend_from_slice("❯".as_bytes());
        screen.resize(4096, b'a');
        assert_eq!(screen.len(), 4096);
        assert_eq!(classify(&screen, State::Idle, false), State::Idle);
    }

    /// Codex's approval prompt, shaped as the review describes it: numbered
    /// options under a `›` selection cursor. Unverified against a live Codex
    /// CLI, which this machine does not have.
    const CODEX_APPROVAL: &str = "  Would you like to run the following command?\n\
         \n\
         \u{203a} 1. Yes, proceed (y)\n\
           2. Yes, and don't ask again for this command (a)\n\
           3. No, and tell Codex what to do differently (esc)";

    #[test]
    fn codex_approval_with_its_cursor_matches() {
        assert!(matches_prompt(CODEX_APPROVAL));
    }

    #[test]
    fn codex_approval_without_its_cursor_does_not_match() {
        // The same block with the selection cursor gone: numbered options
        // alone are not a prompt.
        assert!(!matches_prompt(&CODEX_APPROVAL.replace('\u{203a}', "")));
    }

    #[test]
    fn composer_cursor_before_numbered_output_does_not_match() {
        // Codex's composer draws `› ` too, so a bare containment check would
        // read a working session's numbered output as a prompt.
        let tail = "\u{203a} run the tests\n1. compiling crate\n2. linking crate\n";
        assert!(!matches_prompt(tail));
    }

    #[test]
    fn typed_cursor_with_numbered_output_elsewhere_does_not_match() {
        // A `› 1.` the user typed, with numbered output later in the tail:
        // the second option must follow the cursor's option line directly.
        let tail = "\u{203a} 1. let me check\n\nHere is the plan:\n\
                    1. compiling crate\n2. linking crate\n";
        assert!(!matches_prompt(tail));
    }

    #[test]
    fn plain_numbered_output_does_not_match() {
        let output = "Build output:\n1. compiling crate\n2. linking crate\n";
        assert!(!matches_prompt(output));
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
    use super::{
        IDLE_AFTER, State, WatchStreams, on_plain_shell_transition, on_transition, record_status,
        watch, watch_plain_shell,
    };
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
            bin_templates: None,
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
            status_since: Some(Utc::now()),
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
            spawn_origin: None,
            claude_session_id: None,
            agent_conversation_id: None,
            pending_input: None,
            last_hook_time: None,
            hook_driven: false,
            last_assistant_message: None,
            busy_excluded: false,
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
        start_from(tag, plain_shell, SessionStatus::Idle)
    }

    /// Start a watcher over a record that already carries `initial`, the way a
    /// session reattached from its sidecar does.
    fn start_from(tag: &str, plain_shell: bool, initial: SessionStatus) -> Harness {
        let mut record = test_record(SESSION);
        record.status = initial;
        start_record(tag, plain_shell, record)
    }

    /// Start a watcher over `record` as the session's spawn or reattach left
    /// it.
    fn start_record(tag: &str, plain_shell: bool, record: SessionRecord) -> Harness {
        let initial = record.status;
        let registry = SessionRegistry::new(scratch_dirs(tag));
        registry.insert(record);
        let (output, output_rx) = broadcast::channel(64);
        let (input, input_rx) = mpsc::unbounded_channel();
        let (attention_tx, attention_rx) = mpsc::unbounded_channel();
        if plain_shell {
            watch_plain_shell(
                &registry,
                SESSION.to_string(),
                WatchStreams {
                    output: output_rx,
                    input_pulses: input_rx,
                },
                initial,
            );
        } else {
            watch(
                &registry,
                SESSION.to_string(),
                WatchStreams {
                    output: output_rx,
                    input_pulses: input_rx,
                },
                attention_tx,
                initial,
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

        /// Change the session record the way a hook event does while the
        /// watcher is parked.
        fn set_record(&self, f: impl FnOnce(&mut SessionRecord)) {
            self.registry.update(SESSION, f);
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

    /// A plain shell never waits for input, and its idle timer only fires from
    /// `Working`: seeded `AwaitingInput` it would never leave it.
    #[test]
    fn plain_shell_seeded_awaiting_input_starts_idle() {
        assert_eq!(
            State::seed_plain_shell(SessionStatus::AwaitingInput),
            State::Idle
        );
        assert_eq!(
            State::seed_plain_shell(SessionStatus::Working),
            State::Working
        );
        assert_eq!(State::seed_plain_shell(SessionStatus::Idle), State::Idle);
    }

    /// A session reattached `AwaitingInput` has to settle back to `Idle` once
    /// the PTY goes quiet: a watcher started `Idle` never arms the idle timer
    /// for it, so the restored status would stick until the agent's next byte.
    #[tokio::test(start_paused = true)]
    async fn restored_awaiting_input_demotes_after_silence() {
        let h = start_from("restored-awaiting", false, SessionStatus::AwaitingInput);
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the restored status is the watcher's seed"
        );
        // A redraw below the echo budget that matches no prompt either: only
        // the idle timer can move this session.
        h.output
            .send(b"\x1b[2K\r$ ".to_vec())
            .expect("watcher still subscribed");
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a small redraw leaves it waiting"
        );
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence past IDLE_AFTER demotes it"
        );
    }

    /// While hooks own the status, output the classifier would read as work
    /// must not promote a hook-reported `AwaitingInput`.
    #[tokio::test(start_paused = true)]
    async fn hook_awaiting_input_survives_working_output() {
        let h = start_from("hook-survives-output", false, SessionStatus::AwaitingInput);
        h.set_record(|record| record.hook_driven = true);
        for tick in 0..3 {
            h.heartbeat(tick);
            settle().await;
            assert_eq!(
                h.status(),
                SessionStatus::AwaitingInput,
                "tick {tick}: classifier output promoted a hook-reported prompt"
            );
            tokio::time::sleep(HEARTBEAT / 4).await;
        }
    }

    /// Hook-reported work with no further PTY output is an interrupted turn:
    /// the silence demotion still ends it.
    #[tokio::test(start_paused = true)]
    async fn hook_working_demotes_to_idle_after_silence() {
        let h = start("hook-working-silence", false);
        h.set_record(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::Working;
        });
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence demotes hook-reported work"
        );
    }

    /// While hooks own a plain shell, output the shell classifier would read
    /// as work must not promote a hook-reported `Idle`.
    #[tokio::test(start_paused = true)]
    async fn hook_plain_shell_idle_survives_working_output() {
        let h = start("shell-hook-idle", true);
        h.set_record(|record| record.hook_driven = true);
        for tick in 0..3 {
            h.heartbeat(tick);
            settle().await;
            assert_eq!(
                h.status(),
                SessionStatus::Idle,
                "tick {tick}: shell output promoted a hook-reported idle"
            );
            tokio::time::sleep(HEARTBEAT / 4).await;
        }
    }

    /// Hook-reported work in a plain shell with no further PTY output is an
    /// interrupted turn: the silence demotion still ends it.
    #[tokio::test(start_paused = true)]
    async fn hook_plain_shell_working_demotes_to_idle_after_silence() {
        let h = start("shell-hook-working", true);
        h.set_record(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::Working;
        });
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence demotes hook-reported work"
        );
    }

    /// A hook-driven `Working` whose CLI has ended must still demote: the
    /// clear leaves `hook_driven` false, and the local state may never have
    /// seen the `Working` chunk.
    #[tokio::test(start_paused = true)]
    async fn plain_shell_demotes_a_working_record_after_its_hooks_end() {
        let h = start("shell-hooks-ended", true);
        h.set_record(|record| {
            record.status = SessionStatus::Working;
            record.hook_driven = false;
        });
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence demotes a Working left by ended hooks"
        );
    }

    /// The shell classifier never overwrites a hook-reported prompt, and the
    /// plain-shell watcher carries no prompt matcher for the silence
    /// demotion, so it holds.
    #[tokio::test(start_paused = true)]
    async fn hook_plain_shell_awaiting_input_survives_output() {
        let h = start("shell-hook-awaiting", true);
        h.set_record(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::AwaitingInput;
        });
        for tick in 0..3 {
            h.heartbeat(tick);
            settle().await;
            assert_eq!(
                h.status(),
                SessionStatus::AwaitingInput,
                "tick {tick}: shell output overwrote a hook-reported prompt"
            );
            tokio::time::sleep(HEARTBEAT / 4).await;
        }
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the plain shell leaves a hook-reported prompt alone"
        );
    }

    /// A plain shell without hooks still promotes on its own output.
    #[tokio::test(start_paused = true)]
    async fn non_hook_plain_shell_promotes_on_its_own_output() {
        let h = start("shell-promote", true);
        assert_eq!(h.status(), SessionStatus::Idle);
        h.heartbeat(0);
        settle().await;
        assert_eq!(h.status(), SessionStatus::Working);
    }

    /// A hook-reported prompt whose text has left the screen is dismissed:
    /// the same silence demotes it.
    #[tokio::test(start_paused = true)]
    async fn hook_awaiting_input_demotes_once_the_prompt_is_gone() {
        let h = start("hook-prompt-gone", false);
        h.set_record(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::AwaitingInput;
        });
        h.output
            .send(b"\x1b[2K\r$ ".to_vec())
            .expect("watcher still subscribed");
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a small redraw leaves it waiting"
        );
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence with no prompt demotes it"
        );
    }

    /// A hook-reported prompt still on screen holds past `IDLE_AFTER`: the
    /// demotion reads the screen, not the silence alone.
    #[tokio::test(start_paused = true)]
    async fn hook_awaiting_input_holds_while_the_prompt_matches() {
        let h = start("hook-prompt-holds", false);
        h.set_record(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::AwaitingInput;
        });
        h.output
            .send(b"\x1b[2K\rDo you want to proceed? 1. Yes 2. No ".to_vec())
            .expect("watcher still subscribed");
        settle().await;
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a prompt on screen holds the demotion"
        );
    }

    /// A real `AskUserQuestion` screen carries markers outside ASCII: the
    /// `❯` cursor with numbered options must hold the demotion as surely as
    /// the ASCII "Do you want to" wording does.
    #[tokio::test(start_paused = true)]
    async fn hook_awaiting_input_holds_on_a_chevron_question() {
        let h = start("hook-chevron-holds", false);
        h.set_record(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::AwaitingInput;
        });
        h.output
            .send("\x1b[2K\r❯ 1. Yes  2. No ".as_bytes().to_vec())
            .expect("watcher still subscribed");
        settle().await;
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a chevron question on screen holds the demotion"
        );
    }

    /// A session with no hook events keeps the heuristic's own classifier:
    /// it still promotes on activity and demotes on silence.
    #[tokio::test(start_paused = true)]
    async fn hook_awaiting_input_is_not_demoted_without_hooks() {
        let h = start_from("heuristic-intact", false, SessionStatus::AwaitingInput);
        h.heartbeat(0);
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the classifier still promotes on output"
        );
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the classifier still demotes on silence"
        );
    }

    /// The status record `id`'s registry holds, failing when it is gone.
    fn status_in(registry: &SessionRegistry) -> SessionStatus {
        let record = registry.get(SESSION).expect("session registered");
        record.lock().expect("record lock").status
    }

    /// A hook event that lands between the classifier's read of the record
    /// and its write must keep the hook's status: the write is refused because
    /// the hook-driven flag or the hook stamp moved since that read.
    #[test]
    fn a_hook_landing_after_the_read_keeps_its_awaiting_input() {
        let registry = SessionRegistry::new(scratch_dirs("hook-race-agent"));
        registry.insert(test_record(SESSION));
        let read = record_status(&registry, SESSION).expect("the session is registered");
        assert!(!read.hook_driven, "the read saw no hooks");
        // A hook event lands after the read and before the classifier writes.
        registry.update(SESSION, |record| {
            record.status = SessionStatus::AwaitingInput;
            record.hook_driven = true;
            record.last_hook_time = Some(7);
        });
        let mut state = State::Idle;
        let (attention, _receiver) = mpsc::unbounded_channel();
        on_transition(
            &registry,
            SESSION,
            &mut state,
            State::Working,
            read,
            &attention,
        );
        assert_eq!(
            status_in(&registry),
            SessionStatus::AwaitingInput,
            "the classifier overwrote a hook prompt that landed after its read"
        );
    }

    /// The plain shell's write carries no stamp check at all: a hook event
    /// landing after the classifier's read must survive that write too.
    #[test]
    fn a_hook_landing_after_the_plain_shell_read_keeps_its_awaiting_input() {
        let registry = SessionRegistry::new(scratch_dirs("hook-race-shell"));
        registry.insert(test_record(SESSION));
        let read = record_status(&registry, SESSION).expect("the session is registered");
        assert!(!read.hook_driven, "the read saw no hooks");
        registry.update(SESSION, |record| {
            record.status = SessionStatus::AwaitingInput;
            record.hook_driven = true;
            record.last_hook_time = Some(7);
        });
        let mut state = State::Idle;
        on_plain_shell_transition(&registry, SESSION, &mut state, State::Working, read);
        assert_eq!(
            status_in(&registry),
            SessionStatus::AwaitingInput,
            "the shell classifier's unguarded write overwrote a hook prompt"
        );
    }

    /// The guard refuses only a stale read: a transition taken with the record
    /// unchanged since the read still lands.
    #[test]
    fn a_write_after_the_read_still_applies_without_a_hook() {
        let registry = SessionRegistry::new(scratch_dirs("classifier-write"));
        registry.insert(test_record(SESSION));
        let read = record_status(&registry, SESSION).expect("the session is registered");
        let mut state = State::Idle;
        let (attention, _receiver) = mpsc::unbounded_channel();
        on_transition(
            &registry,
            SESSION,
            &mut state,
            State::Working,
            read,
            &attention,
        );
        assert_eq!(status_in(&registry), SessionStatus::Working);
    }
}
